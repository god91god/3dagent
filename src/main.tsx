import ReactDOM from "react-dom/client";
import App from "./App";

// 不用 StrictMode：开发模式下它会让 useEffect 双挂载，与异步 3D 模型/动画加载竞态，
// 导致动画时好时坏（模型加载回调在清理后仍执行，覆盖 mixer 状态）
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <App />,
);
