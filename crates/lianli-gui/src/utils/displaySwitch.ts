import type { DeviceInfo } from "@/types";

export const DISPLAY_SWITCH_TIMEOUT_MS = 60_000;

function identity(device: DeviceInfo): string {
  const port = /^hid:[0-9a-f]{4}:[0-9a-f]{4}:(.+)$/i.exec(device.device_id);
  return port ? `port:${port[1]}` : device.device_id;
}

export function displaySwitchComplete(source: DeviceInfo, devices: DeviceInfo[]): boolean {
  return devices.some((candidate) => identity(candidate) === identity(source)
    && candidate.family.endsWith("Desktop") !== source.family.endsWith("Desktop")
    && ["HydroShift2Lcd", "HydroShift2OledCurveLcd", "HydroShift2LcdDesktop", "Lancool207", "Lancool207Desktop", "UniversalScreen", "UniversalScreenDesktop", "Vision9p2", "Vision9p2Desktop"].includes(candidate.family));
}
