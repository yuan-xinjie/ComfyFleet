import ReactDOM from 'react-dom/client'
import App from './App'
import './styles.css'
import '@xterm/xterm/css/xterm.css'

// 字体直接用系统已安装的字体库，不再打包任何字体文件。

// 不使用 StrictMode：开发模式下会双挂载 effect，导致 PTY 会话被创建两次。
ReactDOM.createRoot(document.getElementById('root')!).render(<App />)