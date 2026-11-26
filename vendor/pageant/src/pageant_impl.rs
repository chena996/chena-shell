use std::io::IoSlice;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::BytesMut;
use delegate::delegate;
use log::debug;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS,
    PAGE_READWRITE,
};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SendMessageA, WM_COPYDATA};

#[derive(Error, Debug)]
pub enum Error {
    #[error("Pageant not found")]
    NotFound,

    #[error("Buffer overflow")]
    Overflow,

    #[error("No response from Pageant")]
    NoResponse,

    #[error(transparent)]
    WindowsError(#[from] windows::core::Error),
}

impl Error {
    fn from_win32() -> Self {
        Self::WindowsError(windows::core::Error::from_win32())
    }
}

/// Pageant transport stream. Implements [AsyncRead] and [AsyncWrite].
pub struct PageantStream {
    stream: DuplexStream,
}

impl PageantStream {
    pub fn new() -> Self {
        let (one, mut two) = tokio::io::duplex(_AGENT_MAX_MSGLEN * 100);

        let cookie = rand::random::<u64>().to_string();
        tokio::spawn(async move {
            let mut buf = BytesMut::new();
            while let Ok(n) = two.read_buf(&mut buf).await {
                if n == 0 {
                    break;
                }
                if buf.len() < 4 {
                    continue;
                }
                let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                if len.saturating_add(4) > _AGENT_MAX_MSGLEN {
                    debug!("dropping oversized Pageant request: {len} bytes");
                    break;
                }
                if buf.len() < len + 4 {
                    continue;
                }
                let msg = buf.split_to(len + 4).freeze();
                let Ok(response) = query_pageant_direct(cookie.clone(), &msg).map_err(|e| {
                    debug!("Pageant query failed: {:?}", e);
                    e
                }) else {
                    break;
                };
                two.write_all(&response).await?
            }
            std::io::Result::Ok(())
        });

        Self { stream: one }
    }
}

impl Default for PageantStream {
    fn default() -> Self {
        Self::new()
    }
}

impl AsyncRead for PageantStream {
    delegate! {
        to Pin::new(&mut self.stream) {
            fn poll_read(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<Result<(), std::io::Error>>;
        }
    }
}

impl AsyncWrite for PageantStream {
    delegate! {
        to Pin::new(&mut self.stream) {
            fn poll_write(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<Result<usize, std::io::Error>>;

            fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>>;

            fn poll_write_vectored(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                bufs: &[IoSlice<'_>],
            ) -> Poll<Result<usize, std::io::Error>>;

            fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>>;
        }

        to Pin::new(&self.stream) {
            fn is_write_vectored(&self) -> bool;
        }
    }
}

struct MemoryMap {
    filemap: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    length: usize,
    pos: usize,
}

impl MemoryMap {
    fn new(name: String, length: usize) -> Result<Self, Error> {
        let filemap = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                // Pageant validates that the mapping owner is the current user.
                None,
                PAGE_READWRITE,
                0,
                length as u32,
                &HSTRING::from(name),
            )
        }?;
        if filemap.is_invalid() {
            return Err(Error::from_win32());
        }
        let view = unsafe { MapViewOfFile(filemap, FILE_MAP_WRITE, 0, 0, 0) };
        Ok(Self {
            filemap,
            view,
            length,
            pos: 0,
        })
    }

    fn seek(&mut self, pos: usize) {
        self.pos = pos;
    }

    fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        if self.pos + data.len() > self.length {
            return Err(Error::Overflow);
        }

        if data.is_empty() {
            return Ok(());
        }

        unsafe {
            #[allow(clippy::indexing_slicing)]
            std::ptr::copy_nonoverlapping(
                &data[0] as *const u8,
                self.view.Value.add(self.pos) as *mut u8,
                data.len(),
            );
        }
        self.pos += data.len();
        Ok(())
    }

    fn read(&mut self, n: usize) -> Result<Vec<u8>, Error> {
        if self.pos.checked_add(n).map_or(true, |end| end > self.length) {
            return Err(Error::Overflow);
        }
        let out = vec![0; n];
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.view.Value.add(self.pos) as *const u8,
                out.as_ptr() as *mut u8,
                n,
            );
        }
        self.pos += n;
        Ok(out)
    }
}

impl Drop for MemoryMap {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.filemap);
        }
    }
}

fn find_pageant_window() -> Result<HWND, Error> {
    let w = unsafe { FindWindowW(&HSTRING::from("Pageant"), &HSTRING::from("Pageant")) }?;
    if w.is_invalid() {
        return Err(Error::NotFound);
    }
    Ok(w)
}

const _AGENT_COPYDATA_ID: u64 = 0x804E50BA;
const _AGENT_MAX_MSGLEN: usize = 8192;

pub fn is_pageant_running() -> bool {
    find_pageant_window().is_ok()
}

/// Send a one-off query to Pageant and return a response.
pub fn query_pageant_direct(cookie: String, msg: &[u8]) -> Result<Vec<u8>, Error> {
    let hwnd = find_pageant_window()?;
    let map_name = format!("PageantRequest{cookie}");

    let mut map = MemoryMap::new(map_name.clone(), _AGENT_MAX_MSGLEN)?;
    map.write(msg)?;

    let mut char_buffer = map_name.as_bytes().to_vec();
    char_buffer.push(0);
    let cds = COPYDATASTRUCT {
        dwData: _AGENT_COPYDATA_ID as usize,
        cbData: char_buffer.len() as u32,
        lpData: char_buffer.as_ptr() as *mut _,
    };

    let response = unsafe {
        SendMessageA(
            hwnd,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(&cds as *const _ as isize),
        )
    };

    if response.0 == 0 {
        return Err(Error::NoResponse);
    }

    map.seek(0);
    let mut buf = map.read(4)?;
    #[allow(clippy::indexing_slicing)]
    let size = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    buf.extend(map.read(size)?);

    Ok(buf)
}
