import { defineStore } from "pinia";
import { ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import type { ServiceReport } from "@/types/installation";

export const useInstallationStore = defineStore("installation", () => {
  const report = ref<{ services: ServiceReport } | null>(null);
  const checking = ref(false);
  const error = ref("");
  let pending: Promise<void> | null = null;

  function recheck(): Promise<void> {
    if (pending) return pending;
    checking.value = true;
    error.value = "";
    pending = invoke<ServiceReport>("service_report")
      .then((services) => { report.value = { services }; })
      .catch((reason: unknown) => { error.value = String(reason); })
      .finally(() => {
        checking.value = false;
        pending = null;
      });
    return pending;
  }

  return { report, checking, error, recheck };
});
