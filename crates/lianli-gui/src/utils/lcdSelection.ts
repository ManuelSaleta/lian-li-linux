import type { DeviceInfo, LcdConfig } from "@/types";

export function resolveLcdDevice(entry: LcdConfig, devices: DeviceInfo[]): DeviceInfo | undefined {
  if (entry.serial) {
    const wanted = entry.serial.replace(/^hid:/, "");
    return devices.find((device) => device.device_id.replace(/^hid:/, "") === wanted);
  }
  return devices[entry.index ?? 0];
}
