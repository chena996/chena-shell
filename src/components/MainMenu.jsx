import { useEffect, useRef, useState } from 'react';

export default function MainMenu({ commands, onActionError }) {
  const [open, setOpen] = useState(false);
  const [activeIndex, setActiveIndex] = useState(0);
  const triggerRef = useRef(null);
  const menuRef = useRef(null);
  const itemRefs = useRef([]);

  useEffect(() => {
    if (!open) return;
    itemRefs.current[activeIndex]?.focus();
  }, [open, activeIndex]);

  useEffect(() => {
    if (!open) return undefined;
    function handleMouseDown(event) {
      if (!menuRef.current?.contains(event.target) && !triggerRef.current?.contains(event.target)) setOpen(false);
    }
    function handleKeyDown(event) {
      if (event.key === 'Escape') {
        event.preventDefault();
        event.stopPropagation();
        setOpen(false);
        triggerRef.current?.focus();
      } else if (event.ctrlKey && !event.altKey && (event.key.toLocaleLowerCase() === 'k' || event.code === 'KeyK')) {
        setOpen(false);
      }
    }
    document.addEventListener('mousedown', handleMouseDown);
    document.addEventListener('keydown', handleKeyDown, true);
    return () => {
      document.removeEventListener('mousedown', handleMouseDown);
      document.removeEventListener('keydown', handleKeyDown, true);
    };
  }, [open]);

  function moveActive(direction) {
    if (!commands.length) return;
    let next = activeIndex;
    for (let count = 0; count < commands.length; count += 1) {
      next = (next + direction + commands.length) % commands.length;
      if (!commands[next].disabled) {
        setActiveIndex(next);
        return;
      }
    }
  }

  function handleMenuKeyDown(event) {
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      moveActive(1);
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      moveActive(-1);
    } else if (event.key === 'Home' || event.key === 'End') {
      event.preventDefault();
      const index = event.key === 'Home'
        ? commands.findIndex(command => !command.disabled)
        : commands.findLastIndex(command => !command.disabled);
      if (index >= 0) setActiveIndex(index);
    } else if (event.key === 'Tab') {
      setOpen(false);
      triggerRef.current?.focus();
    }
  }

  function execute(command) {
    if (command.disabled) return;
    setOpen(false);
    triggerRef.current?.focus();
    try {
      Promise.resolve(command.run()).catch(onActionError);
    } catch (error) {
      onActionError(error);
    }
  }

  return (
    <div className="app-menu">
      <button
        ref={triggerRef}
        type="button"
        className="app-icon-button"
        aria-label="应用菜单"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls="app-main-menu"
        title="应用菜单"
        onClick={() => { setActiveIndex(0); setOpen(value => !value); }}
        onKeyDown={event => {
          if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
            event.preventDefault();
            setActiveIndex(event.key === 'ArrowDown' ? 0 : Math.max(0, commands.length - 1));
            setOpen(true);
          }
        }}
      >
        <span className="material-icons-round" aria-hidden="true">more_vert</span>
      </button>
      {open && (
        <div id="app-main-menu" ref={menuRef} className="app-menu__popover" role="menu" aria-label="应用操作" onKeyDown={handleMenuKeyDown}>
          {commands.map((command, index) => (
            <button
              key={command.id}
              ref={element => { itemRefs.current[index] = element; }}
              type="button"
              role="menuitem"
              tabIndex={index === activeIndex ? 0 : -1}
              disabled={command.disabled}
              className="app-menu__item"
              onFocus={() => setActiveIndex(index)}
              onClick={() => execute(command)}
            >
              <span className="material-icons-round" aria-hidden="true">{command.icon || 'terminal'}</span>
              <span>{command.label}</span>
              {command.shortcut && <kbd>{command.shortcut}</kbd>}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
