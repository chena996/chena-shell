import { useEffect, useId, useMemo, useRef, useState } from 'react';

function searchableText(command) {
  return `${command.label} ${command.description || ''} ${command.keywords || ''} ${command.group || ''}`.toLocaleLowerCase();
}

export default function CommandPalette({ commands, onClose, onActionError }) {
  const [query, setQuery] = useState('');
  const [activeIndex, setActiveIndex] = useState(0);
  const inputRef = useRef(null);
  const dialogRef = useRef(null);
  const previousFocusRef = useRef(document.activeElement);
  const idPrefix = useId().replace(/:/g, '');
  const listId = `${idPrefix}-commands`;
  const filtered = useMemo(() => {
    const needle = query.trim().toLocaleLowerCase();
    if (!needle) return commands;
    return commands.filter(command => searchableText(command).includes(needle));
  }, [commands, query]);
  const firstEnabled = filtered.findIndex(command => !command.disabled);
  const resolvedActiveIndex = filtered[activeIndex] && !filtered[activeIndex].disabled
    ? activeIndex
    : firstEnabled;
  const activeCommand = filtered[resolvedActiveIndex];
  const activeOptionId = activeCommand ? `${idPrefix}-${activeCommand.id}` : undefined;

  useEffect(() => {
    const previousFocus = previousFocusRef.current;
    inputRef.current?.focus();
    return () => previousFocus?.focus?.();
  }, []);

  useEffect(() => {
    function handleKeyDown(event) {
      if (event.key === 'Escape') {
        event.preventDefault();
        event.stopPropagation();
        onClose();
        return;
      }
      if (event.key !== 'Tab' || !dialogRef.current) return;
      const focusable = Array.from(dialogRef.current.querySelectorAll('button:not([disabled]), input:not([disabled])'));
      if (!focusable.length) return;
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    }
    document.addEventListener('keydown', handleKeyDown, true);
    return () => document.removeEventListener('keydown', handleKeyDown, true);
  }, [onClose]);

  useEffect(() => {
    if (!activeOptionId) return;
    document.getElementById(activeOptionId)?.scrollIntoView?.({ block: 'nearest' });
  }, [activeOptionId]);

  function moveActive(direction) {
    if (!filtered.length || firstEnabled < 0) return;
    let next = resolvedActiveIndex;
    for (let count = 0; count < filtered.length; count += 1) {
      next = (next + direction + filtered.length) % filtered.length;
      if (!filtered[next].disabled) {
        setActiveIndex(next);
        return;
      }
    }
  }

  function moveToEdge(edge) {
    const index = edge === 'first'
      ? filtered.findIndex(command => !command.disabled)
      : filtered.findLastIndex(command => !command.disabled);
    if (index >= 0) setActiveIndex(index);
  }

  function execute(command) {
    if (!command || command.disabled) return;
    onClose();
    try {
      Promise.resolve(command.run()).catch(onActionError);
    } catch (error) {
      onActionError(error);
    }
  }

  function handleInputKeyDown(event) {
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      moveActive(1);
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      moveActive(-1);
    } else if (event.key === 'Home') {
      event.preventDefault();
      moveToEdge('first');
    } else if (event.key === 'End') {
      event.preventDefault();
      moveToEdge('last');
    } else if (event.key === 'Enter') {
      event.preventDefault();
      execute(activeCommand);
    }
  }

  return (
    <div className="command-palette__backdrop" onMouseDown={event => {
      if (event.target === event.currentTarget) onClose();
    }}>
      <section
        ref={dialogRef}
        className="command-palette"
        role="dialog"
        aria-modal="true"
        aria-labelledby={`${idPrefix}-title`}
      >
        <h2 id={`${idPrefix}-title`} className="sr-only">命令面板</h2>
        <div className="command-palette__search">
          <span className="material-icons-round" aria-hidden="true">search</span>
          <input
            ref={inputRef}
            type="text"
            role="combobox"
            aria-label="搜索命令"
            aria-autocomplete="list"
            aria-expanded="true"
            aria-controls={listId}
            aria-activedescendant={activeOptionId}
            value={query}
            onChange={event => { setQuery(event.target.value); setActiveIndex(0); }}
            onKeyDown={handleInputKeyDown}
            placeholder="搜索会话或执行命令…"
            spellCheck={false}
          />
          <kbd>Ctrl K</kbd>
          <button type="button" className="command-palette__close" onClick={onClose} aria-label="关闭命令面板">✕</button>
        </div>
        <div className="command-palette__hint" aria-hidden="true">
          <span>↑↓ 选择</span><span>Enter 执行</span><span>Esc 关闭</span>
        </div>
        <div id={listId} className="command-palette__results" role="listbox" aria-label="可用命令">
          {filtered.map((command, index) => {
            const selected = index === resolvedActiveIndex;
            return (
              <div
                id={`${idPrefix}-${command.id}`}
                key={command.id}
                className={`command-palette__option${selected ? ' is-active' : ''}`}
                role="option"
                aria-selected={selected}
                aria-disabled={command.disabled || undefined}
                onMouseEnter={() => { if (!command.disabled) setActiveIndex(index); }}
                onMouseDown={event => event.preventDefault()}
                onClick={() => execute(command)}
              >
                <span className="material-icons-round command-palette__option-icon" aria-hidden="true">{command.icon || 'terminal'}</span>
                <span className="command-palette__option-copy">
                  <span className="command-palette__option-label">{command.label}</span>
                  {command.description && <span className="command-palette__option-description">{command.description}</span>}
                </span>
                {command.shortcut && <kbd>{command.shortcut}</kbd>}
              </div>
            );
          })}
          {!filtered.length && <div className="command-palette__empty">没有匹配的命令，请尝试会话名、主机或“设置”。</div>}
          {filtered.length > 0 && firstEnabled < 0 && <div className="command-palette__empty">当前没有可执行的命令。</div>}
        </div>
      </section>
    </div>
  );
}
