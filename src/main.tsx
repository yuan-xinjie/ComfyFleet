import ReactDOM from 'react-dom/client'
import App from './App'
import './styles.css'
import '@xterm/xterm/css/xterm.css'

// 自带字体：不依赖系统装了什么，也不在运行时联网
import '@fontsource/inter/400.css'
import '@fontsource/inter/500.css'
import '@fontsource/inter/600.css'
import '@fontsource/inter/700.css'

// 不使用 StrictMode：开发模式下会双挂载 effect，导致 PTY 会话被创建两次。
ReactDOM.createRoot(document.getElementById('root')!).render(<App />)