<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from "vue";
import { open } from "@tauri-apps/plugin-shell";
import { useIpc } from "@/composables/useIpc";
import { useDaemonStore } from "@/stores/daemon";
import type { AssetAccessReport, LcdConfig, LcdTemplate } from "@/types";

const props = defineProps<{ lcds: LcdConfig[]; templates: LcdTemplate[] }>();
const ipc = useIpc();
const daemon = useDaemonStore();
const report = ref<AssetAccessReport | null>(null);
const error = ref("");
const checking = ref(false);
const supported = computed(() => daemon.connected && daemon.info?.capabilities.includes("media_access"));
const hasSelection = computed(() => props.lcds.length > 0 || props.templates.length > 0);
const selection = computed(() => JSON.stringify({ lcds: props.lcds, templates: props.templates }));
let timer: ReturnType<typeof setTimeout> | undefined;
let revision = 0;
let pending = false;
let disposed = false;

function schedule() {
  revision++;
  report.value = null;
  error.value = "";
  pending = false;
  clearTimeout(timer);
  if (!supported.value || !hasSelection.value) return;
  timer = setTimeout(() => {
    pending = true;
    void check();
  }, 600);
}

async function check() {
  if (checking.value || disposed || !pending) return;
  pending = false;
  const current = revision;
  checking.value = true;
  try {
    const result = await ipc.request<AssetAccessReport>("CheckMediaAccess", JSON.parse(selection.value));
    if (!disposed && current === revision) report.value = result;
  } catch (cause) {
    if (!disposed && current === revision) error.value = String(cause);
  } finally {
    checking.value = false;
    if (pending && !disposed) void check();
  }
}

watch([selection, supported, () => daemon.info?.instance_id, () => daemon.socketPath], schedule, { immediate: true });
onUnmounted(() => {
  disposed = true;
  revision++;
  clearTimeout(timer);
});
</script>

<template>
  <div v-if="hasSelection" class="media-access">
    <n-alert v-if="report?.failed" type="warning" title="The daemon cannot use some selected assets">
      <p>Checked using the {{ daemon.info?.mode }} daemon account (UID {{ report.uid }}). Files and their parent folders must be accessible to that account.</p>
      <ul>
        <li v-for="(issue, index) in report.issues" :key="index">
          {{ issue.owner }}<span v-if="issue.path"> — {{ issue.path }}</span>: {{ issue.error }}
        </li>
      </ul>
      <p v-if="report.failed > report.issues.length">Showing {{ report.issues.length }} of {{ report.failed }} issues.</p>
      <n-button size="small" @click="open('https://github.com/sgtaziz/lian-li-linux/blob/main/docs/lcd-assets.md')">File access guide</n-button>
    </n-alert>
    <p v-else-if="error" role="status">Could not check media access: {{ error }}</p>
    <p v-else-if="!daemon.connected" class="muted">Connect to the daemon to check selected media access.</p>
    <p v-else-if="!supported" class="muted">Update and restart the daemon to check selected media access.</p>
    <div v-if="supported" class="access-actions">
      <span class="muted" role="status">
        {{ checking ? "Checking media access…" : report && !report.failed ? `${report.checked} media dependencies readable. Access is checked again when preparing playback.` : "" }}
      </span>
      <n-button size="small" :disabled="checking" @click="schedule">Recheck media access</n-button>
    </div>
  </div>
</template>

<style scoped>
.media-access { overflow-wrap: anywhere; }
.media-access ul { max-height: 10rem; overflow-y: auto; }
.access-actions { display: flex; flex-wrap: wrap; align-items: center; gap: var(--space-3); margin-top: var(--space-2); }
</style>
