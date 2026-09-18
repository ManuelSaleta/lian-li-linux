<script setup lang="ts">
import { useDaemonStore } from "@/stores/daemon";
import { useDevicesStore } from "@/stores/devices";
import { useIpc } from "@/composables/useIpc";
import { computed, ref } from "vue";
import { useMessage } from "naive-ui";
const daemon = useDaemonStore();
const devices = useDevicesStore();
const ipc = useIpc();
const message = useMessage();
const retrying = ref(false);
const clearing = ref<string>();
const failed = computed(() => Object.values(daemon.mediaPreparation).some(status => status.state === "failed" || status.runtime?.stage === "failed"));

async function retry() {
  if (retrying.value || !daemon.canWrite) return;
  retrying.value = true;
  try {
    await ipc.request("RetryMedia");
    message.info("Media retry queued using saved settings.");
    await daemon.refresh();
  } catch (error) {
    message.error(String(error));
  } finally {
    retrying.value = false;
  }
}

async function clearRecovery(deviceId: string) {
  if (clearing.value || !daemon.canWrite) return;
  clearing.value = deviceId;
  try {
    await ipc.request("ClearStartupImageRecovery", { device_id: deviceId });
    message.info("Recovery reset requested. Playback will retry shortly.");
    await daemon.refresh();
  } catch (error) {
    message.error(String(error));
  } finally {
    clearing.value = undefined;
  }
}
</script>

<template>
  <n-button v-if="failed && daemon.info?.capabilities.includes('media_retry')" class="retry" :loading="retrying" :disabled="retrying || !daemon.canWrite" @click="retry">Retry failed media</n-button>
  <div v-if="Object.keys(daemon.mediaPreparation).length" class="stream-grid">
    <n-card v-for="(status, index) in daemon.mediaPreparation" :key="index" size="small" :title="devices.byId(status.device_id)?.name ?? `LCD ${Number(index) + 1}`">
      <template #header-extra><n-tag size="small" :type="status.state === 'failed' || status.runtime?.stage === 'failed' ? 'error' : status.state === 'ready' ? 'success' : 'warning'">{{ status.runtime?.stage === 'failed' ? 'Stopped' : status.state.replaceAll('_', ' ') }}</n-tag></template>
      <dl>
        <div><dt>Serial</dt><dd>{{ devices.byId(status.device_id)?.serial ?? status.device_id }}</dd></div>
        <div><dt>Transfer</dt><dd>{{ !status.runtime ? '—' : status.runtime.h264_transfer_started != null || status.runtime.encoder || status.runtime.stage === 'autonomous_source_configured' ? 'H.264' : status.runtime.stage === 'frame_submitted' ? 'JPEG' : '—' }}</dd></div>
        <div><dt>FPS limit</dt><dd>{{ status.runtime?.fps_limit ?? '—' }}</dd></div>
        <div><dt>Encoder</dt><dd>{{ status.runtime?.encoder?.name ?? '—' }}</dd></div>
      </dl>
      <p v-if="status.error" class="failure">{{ status.error }}</p>
      <n-button v-if="status.startup_recovery_required && daemon.info?.capabilities.includes('startup_recovery_clear')" size="small" :loading="clearing === status.device_id" :disabled="!!clearing || !daemon.canWrite" @click="clearRecovery(status.device_id)">Clear recovery and retry playback</n-button>
      <details v-if="status.last_playback_error || status.runtime">
        <summary>Playback details</summary>
        <p v-if="status.runtime">{{ status.runtime.stage.replaceAll('_', ' ') }} · Hardware video {{ status.runtime.hardware_video_allowed ? 'enabled' : 'disabled' }}</p>
        <p v-if="status.runtime?.h264_transfer_started != null">First H.264 transfer: {{ status.runtime.h264_transfer_started ? 'Sent' : 'Pending' }}</p>
        <p v-if="status.runtime?.fallback_reason">{{ status.runtime.fallback_reason }}</p>
        <p v-if="status.runtime?.encoder?.software_fallback">Hardware video fell back to software encoding.</p>
        <p v-if="status.last_playback_error">Last error: {{ status.last_playback_error }}</p>
        <p v-if="status.runtime?.stage === 'failed' || status.state === 'failed'">{{ daemon.info?.capabilities.includes('media_retry') ? 'Repair the cause, then retry using saved settings.' : 'Repair the cause, then restart the service.' }}</p>
      </details>
    </n-card>
  </div>
</template>

<style scoped>
.retry { margin-bottom: var(--space-3); }
.stream-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(100%, 320px), 1fr)); gap: var(--space-3); }
dl { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: var(--space-3); margin: 0; }
dt, summary { color: var(--text-secondary); }
dd { margin: 2px 0 0; overflow-wrap: anywhere; }
p { margin: var(--space-2) 0; overflow-wrap: anywhere; }
details { margin-top: var(--space-3); }
summary { cursor: pointer; }
.failure { color: var(--danger); }
</style>
