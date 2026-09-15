<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useDialog } from "naive-ui";
import { useConfigStore } from "@/stores/config";
import type { ServiceAction, ServiceChangeRequest, ServiceOperationStatus, ServiceScope } from "@/types/installation";
import { useInstallationStore } from "@/stores/installation";
import { useDaemonStore } from "@/stores/daemon";
import { canSetUpServices } from "@/utils/serviceSetup";

defineProps<{ setupOnly?: boolean }>();

const installation = useInstallationStore();
const daemon = useDaemonStore();
const config = useConfigStore();
const dialog = useDialog();
const operation = ref<ServiceOperationStatus>({ active: false, message: "", success: null });
const submitting = ref(false);
const submissionError = ref("");
const destination = ref<ServiceScope | null>(null);
const settingsChoice = ref(daemon.connected ? "carry" : "destination");
const modeOptions = [
  { label: "User (at login)", value: "user" },
  { label: "System (at boot)", value: "system" },
];
const busy = computed(() => submitting.value || operation.value.active);
let unlisten: UnlistenFn | undefined;
let disposed = false;
let progressEvents = 0;
let timer: ReturnType<typeof setTimeout> | undefined;
let refreshing: Promise<boolean> | undefined;

async function completed() {
  await daemon.refresh();
  if (daemon.connected && !config.dirty) await config.load();
  await installation.recheck();
}

async function refreshOperation() {
  if (refreshing) return refreshing;
  refreshing = (async () => {
    const previous = progressEvents;
    const status = await invoke<ServiceOperationStatus>("service_operation_status");
    if (!disposed && progressEvents === previous) {
      const finished = operation.value.active && !status.active;
      operation.value = status;
      if (finished) await completed();
      return finished;
    }
    return false;
  })();
  try { return await refreshing; }
  finally { refreshing = undefined; }
}

function scheduleProgress() {
  if (disposed) return;
  timer = setTimeout(async () => {
    try { await refreshOperation(); }
    catch (error) { submissionError.value = String(error); }
    finally { scheduleProgress(); }
  }, operation.value.active ? 1000 : 5000);
}

onMounted(async () => {
  if (!installation.report) void installation.recheck();
  try {
    const stop = await listen<ServiceOperationStatus>("service-operation", (event) => {
      progressEvents++;
      const finished = operation.value.active && !event.payload.active;
      operation.value = event.payload;
      if (finished) {
        void completed().catch((error: unknown) => { submissionError.value = String(error); });
      }
    });
    if (disposed) { stop(); return; }
    unlisten = stop;
    await refreshOperation();
  } catch (error) {
    operation.value = { active: false, success: false, message: String(error) };
  } finally { scheduleProgress(); }
});
onUnmounted(() => { disposed = true; unlisten?.(); clearTimeout(timer); });

async function perform(command: "service_action" | "service_change", request: { scope: ServiceScope; action: ServiceAction } | ServiceChangeRequest) {
  submitting.value = true;
  submissionError.value = "";
  try {
    await invoke<void>(command, { request });
    const refreshed = await refreshOperation();
    if (!operation.value.active && !refreshed) await completed();
  } catch (error) {
    submissionError.value = String(error);
  } finally {
    submitting.value = false;
  }
}

function act(scope: ServiceScope, action: ServiceAction) {
  const title = action[0].toUpperCase() + action.slice(1);
  dialog.warning({
    title: `${title} host ${scope} service`,
    content: config.dirty
      ? "Save your pending configuration changes before continuing. Save and close any open template editor first."
      : `${title} the host ${scope} hardware service? Startup selection stays as configured.`,
    positiveText: config.dirty ? "Save and continue" : title,
    negativeText: "Cancel",
    onPositiveClick: async () => {
      if (config.dirty) {
        try { await config.save(); }
        catch (error) {
          operation.value = { active: false, success: false, message: String(error) };
          return false;
        }
      }
      await perform("service_action", { scope, action });
    },
  });
}
function change(request: ServiceChangeRequest) {
  if (busy.value || (request.kind === "switch" && request.scope === currentMode.value)) return;
  const recovery = request.kind === "recover";
  const setup = !recovery && initialSetup.value;
  dialog.warning({
    title: recovery ? "Recover host service switch" : setup ? `Enable host ${request.scope} service` : `Switch to host ${request.scope} service`,
    content: config.dirty
      ? "Save or discard your pending configuration changes before switching or recovering services. Save and close the template editor first."
      : recovery
        ? "Resume the interrupted operation and restore the previous service mode where needed? Administrator authorization is required."
        : setup
          ? `Enable and start the ${request.scope} service? The other mode will stay disabled. Saved settings will be used if available. Administrator authorization is required.`
        : `Stop the previous service cleanly, enable only the ${request.scope} service and start it? ${request.carry_settings
          ? "Copy current settings and media to this account. Back up its existing settings first."
          : "The destination account's saved settings will be used. If none exist, the daemon will use its defaults."} Cooling control briefly pauses while the services change. Administrator authorization is required.`,
    positiveText: config.dirty ? "Close" : recovery ? "Recover" : setup ? "Enable and start" : "Switch service",
    negativeText: "Cancel",
    onPositiveClick: async () => {
      if (!config.dirty) await perform("service_change", request);
    },
  });
}
const report = computed(() => installation.report?.services);
const initialSetup = computed(() => !daemon.connected && canSetUpServices(report.value));
const currentMode = computed<ServiceScope | null>(() => {
  if (daemon.connected && daemon.info?.mode && daemon.info.mode !== "unknown") return daemon.info.mode;
  const selection = report.value?.selection;
  if (selection?.state === "known" && selection.value) return selection.value.scope;
  if (report.value?.user.state === "known" && report.value.user.value.active_state === "active") return "user";
  if (report.value?.system.state === "known" && report.value.system.value.active_state === "active") return "system";
  return null;
});
watch(currentMode, (mode) => { if (mode) destination.value = mode; }, { immediate: true });
const controlsReady = computed(() => report.value?.operation_lock?.state === "known");
const units = computed(() => report.value ? [
  { label: "Host user service", scope: "user" as const, probe: report.value.user },
  { label: "Host system service", scope: "system" as const, probe: report.value.system },
] : []);
const owner = computed(() => {
  const ownership = report.value?.ownership;
  if (ownership?.state !== "known") return "Unverified";
  if (ownership.value.owner_pid === null) return "None";
  const process = ownership.value.process;
  if (!process) return "Process identity has not been checked";
  if (process.state === "unavailable") return `Unverified: ${process.reason}`;
  const label = process.value.service === "user" ? "Your user service"
    : process.value.service === "system" ? "System service"
    : "Manual launch or another service";
  return `${label} (host UID ${process.value.effective_uid})`;
});
const lockMatch = computed(() => {
  const held = daemon.info?.ownership_lock;
  const inspected = report.value?.ownership;
  if (!held || inspected?.state !== "known") return null;
  return held.device === inspected.value.identity.device && held.inode === inspected.value.identity.inode;
});
</script>

<template>
  <n-card :title="initialSetup ? 'Choose a service' : 'Services'" size="small">
    <template #header-extra>
      <n-button size="small" :loading="installation.checking" :disabled="busy" @click="installation.recheck">Recheck</n-button>
    </template>
    <n-alert v-if="submissionError || installation.error" type="error">{{ submissionError || installation.error }}</n-alert>
    <n-alert v-if="operation.message" :type="operation.success === false ? 'error' : operation.success === true ? 'success' : 'info'">{{ operation.message }}</n-alert>
    <p v-if="initialSetup">User mode starts at login. System mode starts at boot. Choose one to enable.</p>
    <p v-if="!report" class="muted">{{ installation.checking ? "Checking services…" : "Service status unavailable." }}</p>
    <p v-if="report?.operation_lock?.state === 'unavailable'">Controls unavailable: {{ report.operation_lock.reason }}</p>
    <div v-if="report?.context.kind === 'native' && controlsReady" class="switch-row">
      <n-select v-model:value="destination" :options="modeOptions" placeholder="Select mode" :disabled="busy || installation.checking" class="mode-select" />
      <n-select v-if="!initialSetup && destination && destination !== currentMode" v-model:value="settingsChoice"
        :options="[{ label: 'Copy current settings & media', value: 'carry' }, { label: 'Use destination settings', value: 'destination' }]"
        :disabled="busy || installation.checking" class="settings-select" />
      <n-button :disabled="busy || installation.checking || !destination || destination === currentMode"
        @click="destination && change({ kind: 'switch', scope: destination, carry_settings: !initialSetup && settingsChoice === 'carry' })">{{ initialSetup ? 'Enable and start' : 'Switch service' }}</n-button>
    </div>
    <p v-else-if="report?.context.kind === 'distrobox'" class="muted">Distrobox uses the host user service. System mode requires a native installation.</p>
    <div v-if="!setupOnly" class="units">
      <div v-for="unit in units" :key="unit.scope" class="unit">
        <div class="unit-heading"><strong>{{ unit.label }}</strong>
          <n-tag v-if="unit.probe.state === 'known'" size="small" :type="unit.probe.value.active_state === 'active' ? 'success' : 'default'">{{ unit.probe.value.active_state }}</n-tag>
        </div>
        <template v-if="unit.probe.state === 'known'">
          <p class="muted">Startup: {{ unit.probe.value.unit_file_state }}<span v-if="unit.probe.value.distrobox_name"> · Daemon: Distrobox ({{ unit.probe.value.distrobox_name }})</span></p>
          <n-space v-if="controlsReady && unit.probe.value.load_state === 'loaded' && (unit.scope === 'user' || report?.context.kind === 'native')">
            <n-button size="small" :disabled="busy || installation.checking || !['inactive', 'failed'].includes(unit.probe.value.active_state)" @click="act(unit.scope, 'start')">Start</n-button>
            <n-button size="small" :disabled="busy || installation.checking || unit.probe.value.active_state !== 'active'" @click="act(unit.scope, 'stop')">Stop</n-button>
            <n-button size="small" :disabled="busy || installation.checking || unit.probe.value.active_state !== 'active'" @click="act(unit.scope, 'restart')">Restart</n-button>
          </n-space>
        </template>
        <p v-else>{{ unit.probe.reason }}</p>
      </div>
    </div>
    <n-alert v-if="daemon.connected && lockMatch === false" type="error">Daemon ownership lock mismatch. Stop competing launch routes before switching services.</n-alert>
    <details v-if="report && !setupOnly">
      <summary>Ownership & recovery</summary>
      <dl v-if="daemon.connected && daemon.info" class="daemon-details">
        <div><dt>Daemon</dt><dd>{{ daemon.info.version }} · {{ daemon.info.mode }} · PID {{ daemon.info.pid }}</dd></div>
        <div><dt>Instance</dt><dd>{{ daemon.info.instance_id }}</dd></div>
        <div class="config-path"><dt>Configuration</dt><dd>{{ daemon.info.config_path }}</dd></div>
      </dl>
      <p>Owner: {{ owner }}</p>
      <p v-if="report.selection?.state === 'known'">Startup selection: {{ report.selection.value?.scope ?? 'Not configured' }}</p>
      <p v-else-if="report.selection?.state === 'unavailable'">{{ report.selection.reason }}</p>
      <p v-if="report.global_user.state === 'known'">Global user startup: {{ report.global_user.value }}</p>
      <p v-else>{{ report.global_user.reason }}</p>
      <p v-if="report.ownership?.state === 'unavailable'">{{ report.ownership.reason }}</p>
      <p v-if="daemon.connected">Ownership lock: {{ lockMatch === true ? 'Matched' : lockMatch === false ? 'Mismatch' : 'Unverified' }}</p>
      <div v-for="unit in units" :key="unit.scope">
        <p v-if="unit.probe.state === 'known'">{{ unit.label }}: {{ unit.probe.value.load_state }} / {{ unit.probe.value.sub_state }} · PID {{ unit.probe.value.main_pid || '—' }}<br />{{ unit.probe.value.fragment_path }}</p>
      </div>
      <n-button v-if="report.context.kind === 'native' && controlsReady" size="small" :disabled="busy || installation.checking" @click="change({ kind: 'recover' })">Recover interrupted switch</n-button>
    </details>
  </n-card>
</template>

<style scoped>
.daemon-details { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(100%, 240px), 1fr)); gap: var(--space-3); }
.daemon-details dt { color: var(--text-secondary); }
.daemon-details dd { margin: 2px 0 0; overflow-wrap: anywhere; }
.config-path { grid-column: 1 / -1; }
.n-alert { margin-bottom: var(--space-3); }
.switch-row, .unit-heading { display: flex; flex-wrap: wrap; align-items: center; gap: var(--space-3); }
.mode-select { width: 180px; }
.settings-select { width: 240px; }
.units { display: grid; grid-template-columns: repeat(auto-fit, minmax(230px, 1fr)); gap: var(--space-3); margin-top: var(--space-3); }
.unit { padding: var(--space-3); border: 1px solid var(--border); border-radius: var(--radius-md); }
.unit-heading { justify-content: space-between; }
p { margin: var(--space-2) 0; overflow-wrap: anywhere; }
details { margin-top: var(--space-3); }
summary { cursor: pointer; color: var(--text-secondary); }
</style>
