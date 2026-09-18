import { open } from "@tauri-apps/plugin-dialog";

const videoExtensions = ["mp4", "webm", "mkv", "mov", "avi", "m4v"];
const filters = {
  image: { name: "Images", extensions: ["png", "jpg", "jpeg", "bmp"] },
  video: { name: "Videos", extensions: videoExtensions },
  gif: { name: "GIF animations", extensions: ["gif"] },
  animation: { name: "Videos and animations", extensions: [...videoExtensions, "gif", "apng", "png"] },
};

export type MediaFileKind = keyof typeof filters;

export function matchesMediaFile(path: string, kind: MediaFileKind): boolean {
  const extension = /\.([^.\\/]+)$/.exec(path)?.[1].toLowerCase() ?? "";
  return filters[kind].extensions.includes(extension);
}

export async function pickMediaFile(kind: MediaFileKind): Promise<string | null> {
  const filter = filters[kind];
  const selected = await open({ multiple: false, directory: false, filters: [filter] });
  if (typeof selected !== "string") return null;
  if (!matchesMediaFile(selected, kind)) {
    throw new Error(`Choose ${filter.name.toLowerCase()}: ${filter.extensions.map(extension => `.${extension}`).join(", ")}.`);
  }
  return selected;
}
