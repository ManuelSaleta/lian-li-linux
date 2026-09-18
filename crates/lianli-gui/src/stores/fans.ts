import { defineStore } from "pinia";
import { onScopeDispose, ref } from "vue";
import { useIpc } from "@/composables/useIpc";
import { useConfigStore } from "@/stores/config";
import type { FanSpeed } from "@/types";

export const useFansStore = defineStore("fans", () => {
  const ipc = useIpc();
  const config = useConfigStore();
  const pendingQuantities = ref<Map<string, number>>(new Map());
  const callbacks = new Map<string, (error?: unknown) => void>();
  let timer: ReturnType<typeof setTimeout> | undefined;
  let running: Promise<void> | undefined;

  async function flushQuantities(): Promise<void> {
    clearTimeout(timer);
    if (running) await running;
    if (!pendingQuantities.value.size) return;
    running = (async () => {
      let failure: unknown;
      while (pendingQuantities.value.size) {
        const [deviceId, quantity] = pendingQuantities.value.entries().next().value!;
        const done = callbacks.get(deviceId);
        pendingQuantities.value.delete(deviceId);
        callbacks.delete(deviceId);
        try {
          await ipc.request("SetEne6k77FanQuantity", { device_id: deviceId, quantity });
          await config.refreshRgbCapabilities();
          done?.();
        } catch (error) {
          done?.(error);
          failure ??= error;
        }
      }
      if (failure) throw failure;
    })();
    try { await running; } finally { running = undefined; }
  }

  const unregister = config.registerFlush(flushQuantities);
  onScopeDispose(() => { clearTimeout(timer); unregister(); });

  function scheduleFanQuantity(
    deviceId: string,
    quantity: number,
    onDone?: (error?: unknown) => void,
  ) {
    pendingQuantities.value.set(deviceId, quantity);
    if (onDone) callbacks.set(deviceId, onDone);
    clearTimeout(timer);
    timer = setTimeout(() => { void flushQuantities().catch(() => {}); }, 400);
  }

  function fanSpeedLabel(speed: FanSpeed, curves: string[]): string {
    if (typeof speed === "number") return `PWM ${Math.round((speed / 255) * 100)}%`;
    if (speed === "__mb_sync__") return "MB Sync";
    if (speed.startsWith("__mb_sync__:")) return `MB Sync (${speed.slice("__mb_sync__:".length)})`;
    if (speed === "off" || speed === "") return "Off";
    if (curves.includes(speed)) return `Curve: ${speed}`;
    return speed;
  }

  return {
    pendingQuantities,
    scheduleFanQuantity,
    fanSpeedLabel,
  };
});
