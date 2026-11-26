import { useEffect, useRef, useState } from 'react';

const STEPS = [
  {
    icon: 'add_link',
    title: '新建一个 SSH 会话',
    description: '点击左侧“新建会话”，填写主机、用户名和认证信息。会话会按分组保存在左侧，便于下次直接使用。',
    detail: '已有会话时，也可以直接点击会话名称进入下一步。',
  },
  {
    icon: 'terminal',
    title: '连接并开始工作',
    description: '选择会话后点击“连接”。终端标签会保留各自的上下文；需要并排查看时，使用终端标签栏的“分屏”。',
    detail: '按 Ctrl+K 可快速切换会话、连接或断开，也可以执行其他常用操作。',
  },
  {
    icon: 'cloud_sync',
    title: '打开 SFTP 管理文件',
    description: '会话连接后，在命令面板执行“打开当前会话 SFTP”，或使用左侧会话的 SFTP 按钮。',
    detail: '终端支持 OSC 7 时，SFTP 会跟随当前活动会话报告的远程目录。',
  },
];

export default function Onboarding({ busy, onFinish, onDismiss }) {
  const [step, setStep] = useState(0);
  const [dontShowAgain, setDontShowAgain] = useState(false);
  const dialogRef = useRef(null);
  const previousFocusRef = useRef(document.activeElement);
  const current = STEPS[step];

  useEffect(() => {
    const previousFocus = previousFocusRef.current;
    dialogRef.current?.focus();
    return () => previousFocus?.focus?.();
  }, []);

  useEffect(() => {
    function handleKeyDown(event) {
      if (event.key === 'Escape' && !busy) {
        event.preventDefault();
        event.stopPropagation();
        onDismiss(dontShowAgain);
        return;
      }
      if (event.key !== 'Tab' || !dialogRef.current) return;
      const focusable = Array.from(dialogRef.current.querySelectorAll('button:not([disabled]), input:not([disabled])'));
      if (!focusable.length) {
        event.preventDefault();
        dialogRef.current.focus();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && (document.activeElement === first || document.activeElement === dialogRef.current)) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    }
    document.addEventListener('keydown', handleKeyDown, true);
    return () => document.removeEventListener('keydown', handleKeyDown, true);
  }, [busy, dontShowAgain, onDismiss]);

  return (
    <div className="onboarding__backdrop">
      <section
        ref={dialogRef}
        className="onboarding"
        role="dialog"
        aria-modal="true"
        aria-labelledby="onboarding-title"
        aria-describedby="onboarding-description"
        tabIndex={-1}
      >
        <div className="onboarding__eyebrow">欢迎使用 Chena-Shell · 约 1 分钟</div>
        <div className="onboarding__progress" aria-label={`第 ${step + 1} 步，共 3 步`}>
          {STEPS.map((item, index) => (
            <span key={item.title} className={index <= step ? 'is-complete' : ''} aria-current={index === step ? 'step' : undefined} />
          ))}
        </div>
        <div className="onboarding__icon" aria-hidden="true">
          <span className="material-icons-round">{current.icon}</span>
        </div>
        <div className="onboarding__step">第 {step + 1} 步，共 3 步</div>
        <h2 id="onboarding-title">{current.title}</h2>
        <p id="onboarding-description" className="onboarding__description">{current.description}</p>
        <p className="onboarding__detail">{current.detail}</p>

        <label className="onboarding__remember">
          <input
            type="checkbox"
            checked={dontShowAgain}
            onChange={event => setDontShowAgain(event.target.checked)}
            disabled={busy}
          />
          不再显示此引导
        </label>
        <div className="onboarding__actions">
          <button type="button" className="onboarding__skip" onClick={() => onDismiss(dontShowAgain)} disabled={busy}>
            {dontShowAgain ? '不再显示并跳过' : '暂时跳过'}
          </button>
          <div className="onboarding__navigation">
            {step > 0 && <button type="button" onClick={() => setStep(value => value - 1)} disabled={busy}>上一步</button>}
            {step < STEPS.length - 1 ? (
              <button type="button" className="onboarding__primary" onClick={() => setStep(value => value + 1)} disabled={busy}>下一步</button>
            ) : (
              <button type="button" className="onboarding__primary" onClick={onFinish} disabled={busy}>
                {busy ? '正在保存…' : '完成并开始使用'}
              </button>
            )}
          </div>
        </div>
      </section>
    </div>
  );
}
