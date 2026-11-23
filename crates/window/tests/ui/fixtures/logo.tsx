import { createRoot } from "react-dom/client";
import "../../../../../src/client/styles.css";
import { AnimatedLogo } from "../../../../../src/client/components.tsx";

createRoot(document.getElementById("root")!).render(
  <AnimatedLogo state="running" />,
);
