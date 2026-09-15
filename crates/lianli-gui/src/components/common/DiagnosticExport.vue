<script setup lang="ts">
import { ref, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { useMessage } from "naive-ui";
import { useInstallationStore } from "@/stores/installation";
import { useDaemonStore } from "@/stores/daemon";

const installation = useInstallationStore();
const daemon = useDaemonStore();
const message = useMessage();
const logSource = ref<string | null>(null);
const options = [
  { label: "Connected daemon (automatic)", value: "auto" },
  { label: "User daemon", value: "user_daemon" },
  { label: "System daemon", value: "system_daemon" },
];
const preview = ref<{ id: number; text: string; can_save: boolean } | null>(null);
const loading = ref(false);
const saving = ref(false);
const error = ref("");
const show = ref(false);
watch(logSource, () => { preview.value = null; });

async function prepare(savedFile = false) {
  if (!installation.report || loading.value) return;
  loading.value = true;
  error.value = "";
  try {
    const result = await invoke<typeof preview.value>(savedFile ? "diagnostic_preview_file" : "diagnostic_preview", { input: {
      report: installation.report, daemon: daemon.info,
      media: daemon.mediaPreparation,
      desktop_streams: daemon.desktopStreams,
      log_source: logSource.value === "auto" ? null : logSource.value,
    } });
    if (result) {
      preview.value = result;
      show.value = true;
    }
  } catch (reason) {
    error.value = String(reason);
  } finally {
    loading.value = false;
  }
}

async function save() {
  if (!preview.value?.can_save || saving.value) return;
  saving.value = true;
  try {
    if (await invoke<boolean>("diagnostic_save", { id: preview.value.id })) {
      message.success("Diagnostic report saved locally");
    }
  } catch (reason) {
    message.error(String(reason));
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <n-card title="Diagnostic export">
    <p>Preview a report from the displayed installation results. Redaction is enabled: configuration, environment, container names and capture-credential fields are excluded, and private details in log text are filtered. Nothing is uploaded.</p>
    <n-form-item label="Required daemon logs">
      <n-select :value="logSource ?? 'auto'" :options="options" :disabled="loading || saving" @update:value="logSource = $event" />
    </n-form-item>
    <p>Includes up to 100 messages from the latest daemon startup. Paths and possible secrets are omitted. For manual launches, choose a saved log. Review the preview before sharing.</p>
    <n-alert v-if="error" type="error">{{ error }}</n-alert>
    <n-space>
      <n-button :loading="loading" :disabled="!installation.report || installation.checking || saving" @click="prepare()">Preview export</n-button>
      <n-button :disabled="loading || saving || !installation.report" @click="prepare(true)">Choose saved daemon log…</n-button>
    </n-space>
    <n-modal v-model:show="show" preset="card" title="Review diagnostic report" class="diagnostic-preview" :mask-closable="!saving" :closable="!saving">
      <p>This is the exact snapshot that will be saved. Recheck Installation Health and generate another preview for newer results.</p>
      <n-alert v-if="preview && !preview.can_save" type="error">
        No usable daemon logs were found. Check the log source and journal access, or choose a saved log. Logs are required to save.
      </n-alert>
      <n-button v-if="preview && !preview.can_save" type="primary" class="saved-log-button" :disabled="loading || saving" @click="prepare(true)">Choose saved daemon log…</n-button>
      <n-input :value="preview?.text ?? ''" type="textarea" readonly :autosize="{ minRows: 12, maxRows: 24 }" />
      <template #footer><n-button type="primary" :loading="saving" :disabled="!preview?.can_save" @click="save">Save JSON…</n-button></template>
    </n-modal>
  </n-card>
</template>

<style scoped>
.diagnostic-preview { width: min(900px, 92vw); }
.n-alert, .saved-log-button { margin-bottom: var(--space-3); }
p { margin: 0 0 var(--space-3); }
</style>
