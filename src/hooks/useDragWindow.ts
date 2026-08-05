import { useCallback, type MouseEventHandler } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";

/**
 * Returns a mousedown handler that initiates window dragging via Tauri API.
 */
export function useDragWindow(): MouseEventHandler {
  return useCallback((e) => {
    // 原生拖动会吞掉后续 dblclick 事件，因此在第二次 mousedown 时直接切换窗口状态。
    if (e.detail >= 2) {
      e.preventDefault();
      void getCurrentWindow().toggleMaximize().catch((error: unknown) => {
        console.error("Failed to toggle window maximize", error);
      });
      return;
    }
    if (e.buttons === 1 && e.detail === 1) {
      void getCurrentWindow().startDragging().catch((error: unknown) => {
        console.error("Failed to start window dragging", error);
      });
    }
  }, []);
}
