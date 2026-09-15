import { defineStore } from "pinia";
import { ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import type { ServiceReport, InstallationFinding } from "@/types/installation";

export const useInstallationStore = defineStore("installation", () => {
  const report = ref<{ services: ServiceReport; findings: InstallationFinding[] } | null>(null);
  const checking = ref(false);
  const error = ref("");
  let pending: Promise<void> | null = null;

  function recheck(): Promise<void> {
    if (pending) return pending;
    checking.value = true;
    error.value = "";
    pending = invoke<{ services: ServiceReport; findings: InstallationFinding[] }>("service_report")
      .then((result) => { report.value = result; })
      .catch((reason: unknown) => { error.value = String(reason); })
      .finally(() => {
        checking.value = false;
        pending = null;
      });
    return pending;
  }

  return { report, checking, error, recheck };
});
