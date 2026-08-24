/* @refresh reload */
import { render } from "solid-js/web";

import { App } from "./App";
import "./styles/theme.css";
import "./styles/app.css";

const root = document.getElementById("root");
if (!root) {
  throw new Error("V index.html chybí #root — bez něj se nemá kam vykreslit.");
}

render(() => <App />, root);
