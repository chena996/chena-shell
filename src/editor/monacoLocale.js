// Monaco 中文语言包。
// 它会把中文词条表挂到 globalThis._VSCODE_NLS_MESSAGES 上；Monaco 的 localize() 在
// 注册命令时就读取该表，所以这段代码必须在任何 monaco 模块（editor.api / 各 contribution）
// 求值之前执行。这里由 main.jsx 入口最前面静态引入，保证启动即生效。
import 'monaco-editor/esm/vs/nls/lang/zh-cn.js';
