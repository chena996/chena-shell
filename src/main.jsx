// Monaco 中文语言包必须最先执行，否则命令面板/右键菜单等标签会在注册时固化为英文。
import './editor/monacoLocale.js';
import React from 'react';
import ReactDOM from 'react-dom/client';
import App from './App.jsx';
import SftpWindow from './components/SftpWindow.jsx';
import EditorWindow from './components/EditorWindow.jsx';
import './index.css';

// Hash 路由：#/ = 主应用，#/sftp?session_id=xxx = SFTP 独立窗口
function Route() {
  const hash = window.location.hash;

  // SFTP 独立窗口
  if (hash.startsWith('#/sftp')) {
    const params = new URLSearchParams(hash.split('?')[1] || '');
    const sessionId = params.get('session_id');
    if (sessionId) {
      return <SftpWindow sessionId={sessionId} />;
    }
  }

  // 内置编辑器独立窗口
  if (hash.startsWith('#/edit')) {
    const params = new URLSearchParams(hash.split('?')[1] || '');
    const sessionId = params.get('session_id');
    const editId = params.get('edit_id');
    if (sessionId && editId) {
      return <EditorWindow sessionId={sessionId} editId={editId} />;
    }
  }

  // 主应用
  return <App />;
}

ReactDOM.createRoot(document.getElementById('root')).render(
  <React.StrictMode>
    <Route />
  </React.StrictMode>
);
