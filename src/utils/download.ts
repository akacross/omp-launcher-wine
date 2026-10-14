import { invoke } from "@tauri-apps/api/tauri";
import { appWindow } from "@tauri-apps/api/window";
import { download as pluginDownload } from "tauri-plugin-upload-api";

type ProgressHandler = (progress: number, total: number) => void;

let wine: Promise<boolean> | undefined;

// tauri-plugin-upload's download, except under Wine: there it stalls just short
// of the end, so it goes through WinHTTP (src-tauri/src/wine_download.rs) with
// the same progress reports.
export const download = async (
  url: string,
  filePath: string,
  progressHandler?: ProgressHandler
): Promise<void> => {
  wine ??= invoke<boolean>("is_wine").catch(() => false);
  if (!(await wine)) {
    return pluginDownload(url, filePath, progressHandler);
  }

  const ids = new Uint32Array(1);
  window.crypto.getRandomValues(ids);
  const id = ids[0];
  const unlisten = await appWindow.listen<{
    id: number;
    progress: number;
    total: number;
  }>("wine_download://progress", ({ payload }) => {
    if (payload.id === id) progressHandler?.(payload.progress, payload.total);
  });
  try {
    await invoke("wine_download", { id, url, filePath });
  } finally {
    // callers finish on the last progress report, which may still be on its way
    setTimeout(unlisten, 1000);
  }
};
