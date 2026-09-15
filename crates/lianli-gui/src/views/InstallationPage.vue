<script setup lang="ts">
import { computed, onMounted } from "vue";
import { open } from "@tauri-apps/plugin-shell";
import { useMessage } from "naive-ui";
import { useDaemonStore } from "@/stores/daemon";
import { useInstallationStore } from "@/stores/installation";
import MediaStreamStatus from "@/components/common/MediaStreamStatus.vue";
import DiagnosticExport from "@/components/common/DiagnosticExport.vue";
import DesktopStreamStatus from "@/components/common/DesktopStreamStatus.vue";
import { INSTALLATION_GUIDES, type CheckState, type InstallationGuide } from "@/types/installation";

const installation = useInstallationStore();
const daemon = useDaemonStore();
const message = useMessage();
const checkedAt = computed(() => installation.report
  ? new Date(installation.report.checked_at_unix_ms).toLocaleString() : "Not checked yet");
const needsAttention = computed(() => (installation.report?.findings ?? []).filter((finding) => finding.state !== "passed" && finding.state !== "not_applicable"));
const otherFindings = computed(() => (installation.report?.findings ?? []).filter((finding) => finding.state === "passed" || finding.state === "not_applicable"));
const stateLabels: Record<CheckState, string> = {
  passed: "Passed", failed: "Failed", unavailable: "Unverified", not_applicable: "N/A",
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
    <div class="connection-row">
      <n-tag :type="daemon.connected ? 'success' : 'warning'">{{ daemon.connected ? 'Connected' : 'Offline' }}</n-tag>
      <span v-if="daemon.info">{{ daemon.info.version }} · {{ daemon.info.mode }}</span>
      <router-link to="/settings">Service settings</router-link>
      <n-button size="small" text type="primary" @click="guide(installation.report?.context.kind === 'distrobox' ? 'distrobox' : 'service_modes')">Setup guide</n-button>
    </div>
    <p v-if="!daemon.connected" class="muted">Start a service in Settings, then Recheck.</p>
    <div class="checks-grid">
      <n-card v-for="finding in needsAttention" :key="finding.code" :title="finding.title" size="small">
        <template #header-extra><n-tag size="small" :type="finding.severity === 'error' ? 'error' : 'warning'">{{ stateLabels[finding.state] }}</n-tag></template>
        <p>{{ finding.remediation || finding.evidence }}</p>
        <details><summary>Details</summary><p class="check-context">{{ finding.context }} · {{ finding.feature }}</p><p>{{ finding.evidence }}</p></details>
        <n-button size="small" text @click="guide(finding.guide)">Open guide</n-button>
      </n-card>
    </div>
    <details v-if="otherFindings.length" class="passed-checks">
      <summary>{{ otherFindings.length }} passed or non-applicable checks</summary>
      <div class="checks-grid">
        <n-card v-for="finding in otherFindings" :key="finding.code" size="small" :title="finding.title">
          <template #header-extra><n-tag size="small" :type="finding.state === 'passed' ? 'success' : 'default'">{{ stateLabels[finding.state] }}</n-tag></template>
          <p class="check-context">{{ finding.feature }}</p>
          <details><summary>Details</summary><p>{{ finding.evidence }}</p><n-button size="small" text @click="guide(finding.guide)">Open guide</n-button></details>
        </n-card>
      </div>
    </details>
    <MediaStreamStatus />
    <DesktopStreamStatus />
    <details class="diagnostics"><summary>Export diagnostics & daemon logs</summary><DiagnosticExport /></details>
    <details><summary>About these checks</summary><p>Checks cover installation, permissions, service state and the last configuration load. Recheck does not reload configuration or test physical playback.</p><p v-if="daemon.socketPath">{{ daemon.socketPath }}</p></details>
  </div>
</template>

<style scoped>
.health-page { display: flex; flex-direction: column; gap: var(--space-4); }
.health-header { display: flex; align-items: center; justify-content: space-between; gap: var(--space-4); }
h1 { font-size: var(--font-size-2xl); margin: 0; }
p { margin: var(--space-3) 0; overflow-wrap: anywhere; }
.check-context { color: var(--text-secondary); }
.checks-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(100%, 320px), 1fr)); gap: var(--space-3); }
.connection-row { display: flex; flex-wrap: wrap; align-items: center; gap: var(--space-3); }
summary { cursor: pointer; color: var(--text-secondary); margin-bottom: var(--space-2); }
</style>
