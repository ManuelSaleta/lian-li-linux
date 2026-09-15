<script setup lang="ts">
import { useDaemonStore } from "@/stores/daemon";
import type { DesktopStreamStatus } from "@/types";
import { ref } from "vue";
import { useMessage } from "naive-ui";
import { useIpc } from "@/composables/useIpc";

const daemon = useDaemonStore();
const ipc = useIpc();
const message = useMessage();
const retrying = ref(false);
async function retry(stream: DesktopStreamStatus) {
  if (retrying.value) return;
  retrying.value = true;
  try {
    await ipc.request("RetryDesktopDisplay", { bus: stream.bus, address: stream.address, product_id: stream.product_id });
    message.info("Retry requested. The daemon will retry after worker cleanup and when the desktop session is ready.");
    await daemon.refresh();
  } catch (error) {
    message.error(String(error));
  } finally { retrying.value = false; }
}
const labels: Record<DesktopStreamStatus["state"], string> = {
  waiting_for_session: "Waiting for desktop session",
  starting: "Starting capture",
  streaming: "Delivering frames",
  paused: "Paused",
  failed: "Failed",
};
</script>

<template>
  <div v-if="daemon.desktopStreams.length" class="stream-grid">
    <n-card v-for="stream in daemon.desktopStreams" :key="`${stream.bus}:${stream.address}`" size="small"
      :title="`Desktop · TURZX ${stream.product_id.toString(16)}`">
      <template #header-extra><n-tag size="small" :type="stream.state === 'failed' ? 'error' : stream.state === 'streaming' ? 'success' : 'warning'">{{ labels[stream.state] }}</n-tag></template>
      <dl>
        <div><dt>USB</dt><dd>{{ stream.bus }}:{{ stream.address }}</dd></div>
        <div><dt>Backend</dt><dd>{{ stream.backend ?? 'Pending' }}</dd></div>
        <div><dt>Transfer</dt><dd>{{ !stream.encoding || stream.encoding.encoder === 'unknown' ? '—' : stream.encoding.encoder === 'turbojpeg' ? 'JPEG' : 'H.264' }}</dd></div>
        <div><dt>FPS limit</dt><dd>{{ stream.applied_policy?.fps_limit ?? '—' }}</dd></div>
        <div><dt>Encoder</dt><dd>{{ stream.encoding?.encoder ?? '—' }}</dd></div>
        <div><dt>Input</dt><dd>{{ !stream.encoding ? '—' : stream.encoding.gpu_input ? 'GPU' : 'CPU' }}</dd></div>
        <div><dt>Hardware video</dt><dd>{{ !stream.applied_policy ? '—' : stream.applied_policy.hardware_video ? 'Enabled' : 'Disabled' }}</dd></div>
      </dl>
      <p v-if="stream.error" class="failure">{{ stream.error }}</p>
      <details v-if="stream.fallback_reason || stream.encoding?.cpu_readback_reason || stream.encoding?.software_reason">
        <summary>Fallback details</summary>
        <p v-if="stream.encoding?.cpu_readback_reason">{{ stream.encoding.cpu_readback_reason === 'no_dma_buf' ? 'This capture backend supports CPU buffers only.' : 'GPU import or encoding failed. Using CPU buffers.' }}</p>
        <p v-if="stream.encoding?.software_reason">{{ stream.encoding.software_reason === 'hardware_failed' ? 'Hardware encoder failed during playback.' : 'No usable hardware encoder.' }}</p>
        <p v-if="stream.fallback_reason">{{ stream.fallback_reason }}</p>
      </details>
      <n-button v-if="stream.state === 'failed' && daemon.info?.capabilities.includes('desktop_retry')" size="small"
        :disabled="!daemon.canWrite || retrying" :loading="retrying" @click="retry(stream)">Retry</n-button>
    </n-card>
  </div>
</template>

<style scoped>
.stream-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(100%, 320px), 1fr)); gap: var(--space-3); }
dl { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: var(--space-3); margin: 0; }
dt, summary { color: var(--text-secondary); }
dd { margin: 2px 0 0; overflow-wrap: anywhere; }
p { margin: var(--space-2) 0; overflow-wrap: anywhere; }
details { margin-top: var(--space-3); }
summary { cursor: pointer; }
.failure { color: var(--danger); }
</style>
