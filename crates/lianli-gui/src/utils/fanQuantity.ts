import type { AppConfig, DeviceInfo } from "@/types";

export function fanQuantityPort(deviceId: string): string | undefined {
  return deviceId.match(/:port([0-3])$/)?.[1];
}

export function fanQuantityKey(deviceId: string): string {
  return deviceId.replace(/:port[0-3]$/, "");
}

export function stageFanQuantity(config: AppConfig, device: DeviceInfo, value: number): number | undefined {
  const port = fanQuantityPort(device.device_id);
  if (port === undefined || !Number.isFinite(value) || !device.max_fan_quantity) return undefined;
  const quantity = Math.max(0, Math.min(device.max_fan_quantity, Math.round(value)));
  const key = fanQuantityKey(device.device_id);
  const controller = config.ene6k77[key] ?? { fan_quantities: {} };
  controller.fan_quantities[port] = quantity;
  config.ene6k77[key] = controller;
  return quantity;
}
