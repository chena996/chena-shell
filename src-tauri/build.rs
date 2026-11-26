fn main() {
    tauri_build::build();

    // tauri-build 默认只为应用 exe 嵌入资源，lib 单元测试也需要 Common Controls v6。
    #[cfg(windows)]
    println!(
        "cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );
}
