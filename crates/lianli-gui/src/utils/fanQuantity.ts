import type { AppConfig, DeviceInfo } from "@/types";

export function fanQuantityPort(deviceId: string): string | undefined {
  return deviceId.match(/:port([0-3])$/)?.[1];
}

export function stageFanQuantity(config: AppConfig, device: DeviceInfo, value: number): number | undefined {
  const port = fanQuantityPort(device.device_id);
  if (port === undefined || !device.serial || !Number.isFinite(value) || !device.max_fan_quantity) return undefined;
  const quantity = Math.max(0, Math.min(device.max_fan_quantity, Math.round(value)));
  const controller = config.ene6k77[device.serial] ?? { fan_quantities: {} };
  controller.fan_quantities[port] = quantity;
  config.ene6k77[device.serial] = controller;
  return quantity;
}
