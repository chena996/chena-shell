/**
 * xterm 主题：严格由 theme prop 决定，绝不从 DOM 读取。
 * theme: 'dark' | 'light'
 */
export function getXtermTheme(theme) {
  const isDark = theme === 'dark';
  if (isDark) {
    // ===== VS Code Dark+ 终端配色（brightBlack 调至对比度 ≥4.5）=====
    return {
      background: '#1E1E1E',
      foreground: '#D4D4D4',
      cursor: '#AEAFAD',
      selectionBackground: '#264F78',
      black: '#000000', red: '#CD3131', green: '#0DBC79', yellow: '#E5E510',
      blue: '#2472C8', magenta: '#BC3FBC', cyan: '#11A8CD', white: '#E5E5E5',
      brightBlack: '#888888', brightRed: '#F14C4C', brightGreen: '#23D18B', brightYellow: '#F5F543',
      brightBlue: '#3B8EEA', brightMagenta: '#D670D6', brightCyan: '#29B8DB', brightWhite: '#FFFFFF',
    };
  }
  // ===== VS Code Light+ 终端配色（brightBlack 调至对比度 ≥4.5）=====
  return {
    background: '#FFFFFF',
    foreground: '#333333',
    cursor: '#234E72',
    selectionBackground: '#ADD6FF',
    black: '#000000', red: '#CD3131', green: '#0DBC79', yellow: '#949800',
    blue: '#0451A5', magenta: '#BC3FBC', cyan: '#11A8CD', white: '#555555',
    brightBlack: '#303030', brightRed: '#CD3131', brightGreen: '#0DBC79', brightYellow: '#949800',
    brightBlue: '#0451A5', brightMagenta: '#D670D6', brightCyan: '#11A8CD', brightWhite: '#A5A5A5',
  };
}

export function terminalBg(theme) {
  return theme === 'dark' ? '#1E1E1E' : '#FFFFFF';
}
