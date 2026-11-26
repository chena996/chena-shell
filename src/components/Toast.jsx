export default function ToastRegion({ error, notice, onDismissError, onDismissNotice }) {
  return (
    <div className="toast-region" aria-label="应用通知">
      {notice && (
        <div className="toast toast--notice" role="status" aria-live="polite" aria-atomic="true">
          <span className="material-icons-round toast__icon" aria-hidden="true">info</span>
          <span className="toast__message">{notice}</span>
          <button type="button" className="toast__close" onClick={onDismissNotice} aria-label="关闭提示">✕</button>
        </div>
      )}
      {error && (
        <div className="toast toast--error" role="alert" aria-live="assertive" aria-atomic="true">
          <span className="material-icons-round toast__icon" aria-hidden="true">error</span>
          <span className="toast__message">{error}</span>
          <button type="button" className="toast__close" onClick={onDismissError} aria-label="关闭错误">✕</button>
        </div>
      )}
    </div>
  );
}
