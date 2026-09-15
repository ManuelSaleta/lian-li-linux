<script setup lang="ts">
import { computed, onMounted } from "vue";
import { open } from "@tauri-apps/plugin-shell";
import { useMessage } from "naive-ui";
import { useDaemonStore } from "@/stores/daemon";
import { useInstallationStore } from "@/stores/installation";
import ServiceStatus from "@/components/common/ServiceStatus.vue";
import DiagnosticExport from "@/components/common/DiagnosticExport.vue";
import DesktopStreamStatus from "@/components/common/DesktopStreamStatus.vue";
import { INSTALLATION_GUIDES, type CheckState, type InstallationGuide } from "@/types/installation";

const installation = useInstallationStore();
const daemon = useDaemonStore();
const message = useMessage();
const checkedAt = computed(() => installation.report
  ? new Date(installation.report.checked_at_unix_ms).toLocaleString() : "Not checked yet");
const stateLabels: Record<CheckState, string> = {
  passed: "Passed", failed: "Failed", unavailable: "Unverified", not_applicable: "Not applicable",
};
onMounted(() => {
  if (!installation.report) void installation.recheck();
});

async function guide(id: InstallationGuide) {
  try {
    await open(INSTALLATION_GUIDES[id]);
  } catch (error) {
    message.error(`Could not open the guide: ${String(error)}`);
  }
}

async function recheck() {
  await Promise.all([installation.recheck(), daemon.refresh()]);
}
</script>

<template>
  <div class="health-page">
    <div class="health-header">
      <div><h1>Installation Health</h1><p>{{ installation.contextLabel }} · {{ checkedAt }}</p></div>
      <n-button :loading="installation.checking" @click="recheck">Recheck</n-button>
    </div>
    <n-alert v-if="installation.error" type="error" title="Check could not finish">
      {{ installation.error }} Previous results, if shown, may be out of date.
    </n-alert>
    <n-card title="Daemon connection">
      <n-tag :type="daemon.connected ? 'success' : 'warning'">
        {{ daemon.connected ? "Connected" : "Waiting for daemon" }}
      </n-tag>
      <p v-if="daemon.info">Version {{ daemon.info.version }} · {{ daemon.info.mode }} mode</p>
      <p v-if="daemon.socketPath">{{ daemon.socketPath }}</p>
      <p v-if="!daemon.connected">
        Fresh installations leave both services stopped. Choose one service mode using the guide.
        If a service is already starting, wait and recheck; if it fails, inspect its journal.
      </p>
      <p v-else-if="!daemon.info">This daemon does not report its identity. Update the daemon and GUI together.</p>
      <n-button @click="guide(installation.report?.context.kind === 'distrobox' ? 'distrobox' : 'service_modes')">
        Open service guide
      </n-button>
    </n-card>
    <ServiceStatus />
    <DesktopStreamStatus />
    <DiagnosticExport />
    <n-card v-for="finding in installation.report?.findings ?? []" :key="finding.code" :title="finding.title">
      <template #header-extra>
        <n-tag :type="finding.state === 'passed' ? 'success' : finding.severity === 'error' ? 'error' : 'warning'">
          {{ stateLabels[finding.state] }}
        </n-tag>
      </template>
      <div class="check-context">{{ finding.context }} · {{ finding.feature }}</div>
      <p>{{ finding.evidence }}</p>
      <p v-if="finding.state !== 'passed'">{{ finding.remediation }}</p>
      <n-button @click="guide(finding.guide)">Open guide</n-button>
    </n-card>
    <n-alert type="info" title="What these checks verify">
      These checks report installation files, service snapshots, daemon credentials, USB/HID node permissions and the last configuration load.
      Node permissions do not prove that driver operations or desktop capture will succeed. Configuration findings describe the last load attempt; Recheck does not reload files or start devices.
    </n-alert>
  </div>
</template>

<style scoped>
.health-page { display: flex; flex-direction: column; gap: var(--space-4); max-width: 1000px; }
.health-header { display: flex; align-items: center; justify-content: space-between; gap: var(--space-4); }
h1 { font-size: var(--font-size-2xl); margin: 0; }
p { margin: var(--space-3) 0; overflow-wrap: anywhere; }
.check-context { color: var(--text-secondary); }
</style>
