<script setup lang="ts">
import { ref, watch } from "vue";
import { useIpc } from "@/composables/useIpc";
import { useDaemonStore } from "@/stores/daemon";

type Target = { kind: "configuration" | "templates" | "rgb_presets" } | { kind: "profile"; name: string };
type Entry = { target: Target; preserved?: boolean; bytes: number; modified_unix_seconds: number | null; error: string | null };
type Preview = { target: Target; preserved?: boolean; sha256: string; bytes: number; json: string; truncated: boolean; parse_error: string | null; validation_error?: string | null; warnings?: string[] };
const daemon = useDaemonStore();
const ipc = useIpc();
const entries = ref<Entry[] | null>(null);
const preview = ref<Preview | null>(null);
const show = ref(false);
const busy = ref(false);
const error = ref("");
const confirmed = ref(false);
const deleteConfirmed = ref(false);
const resultMessage = ref("");
watch(preview, () => { confirmed.value = false; deleteConfirmed.value = false; });
watch(() => daemon.info?.instance_id, () => { entries.value = null; preview.value = null; show.value = false; error.value = ""; resultMessage.value = ""; });
function label(target: Target) {
  return target.kind === "profile" ? `Profile: ${target.name}` : ({ configuration: "Configuration", templates: "LCD templates", rgb_presets: "RGB presets" })[target.kind];
}
async function read(target?: Target, preserved = false) {
  if (busy.value) return;
  busy.value = true;
  const instance = daemon.info?.instance_id;
  error.value = "";
  resultMessage.value = "";
  try {
    if (target) {
      const result = await ipc.request<Preview>("PreviewStateBackup", { target, preserved });
      if (daemon.info?.instance_id !== instance) return;
      preview.value = result;
      show.value = true;
    } else {
      const result = await ipc.request<Entry[]>("ListStateBackups");
      if (daemon.info?.instance_id === instance) entries.value = result;
    }
  } catch (reason) { if (daemon.info?.instance_id === instance) error.value = String(reason); }
  finally { busy.value = false; }
}

async function restore() {
  const selected = preview.value;
  if (!selected || selected.preserved || !confirmed.value || busy.value || !daemon.canWrite) return;
  const instance = daemon.info?.instance_id;
  busy.value = true;
  error.value = "";
  try {
    await ipc.request("RestoreStateBackup", { target: selected.target, sha256: selected.sha256 });
    if (daemon.info?.instance_id !== instance) return;
    show.value = false;
    preview.value = null;
    entries.value = null;
    resultMessage.value = "Backup restored on disk. Daemon reload requested. Review the active settings and any installation or media errors.";
  } catch (reason) { if (daemon.info?.instance_id === instance) error.value = String(reason); }
  finally { busy.value = false; }
}

async function remove() {
  const selected = preview.value;
  if (!selected || !deleteConfirmed.value || busy.value || !daemon.canWrite) return;
  const instance = daemon.info?.instance_id;
  busy.value = true;
  error.value = "";
  try {
    await ipc.request("DeleteStateBackup", { target: selected.target, preserved: selected.preserved ?? false, sha256: selected.sha256 });
    if (daemon.info?.instance_id !== instance) return;
    show.value = false;
    preview.value = null;
    entries.value = null;
    resultMessage.value = "The reviewed backup file was deleted. Current settings were not changed.";
  } catch (reason) { if (daemon.info?.instance_id === instance) error.value = String(reason); }
  finally { busy.value = false; }
}
</script>

<template>
  <n-card title="State backups">
    <p>Inspect the previous saved configuration, templates, RGB presets and profiles belonging to the connected daemon. Previewing does not change settings.</p>
    <n-button :loading="busy" :disabled="!daemon.connected || !daemon.info?.capabilities.includes('backup_preview')" @click="read()">Find backups</n-button>
    <n-alert v-if="error" type="error">{{ error }}</n-alert>
    <n-alert v-if="resultMessage" type="info">{{ resultMessage }}</n-alert>
    <p v-if="entries?.length === 0">No previous-version backups were found.</p>
    <n-space v-for="entry in entries" :key="JSON.stringify([entry.target, entry.preserved])" justify="space-between" align="center">
      <div>{{ label(entry.target) }} · {{ entry.preserved ? 'Before restore' : 'Previous save' }} · {{ entry.bytes.toLocaleString() }} bytes
        <span v-if="entry.modified_unix_seconds"> · {{ new Date(entry.modified_unix_seconds * 1000).toLocaleString() }}</span>
        <p v-if="entry.error">{{ entry.error }}</p>
      </div>
      <n-button :disabled="busy || !!entry.error || !daemon.connected" @click="read(entry.target, entry.preserved)">Preview</n-button>
    </n-space>
    <n-modal v-model:show="show" preset="card" :title="preview ? label(preview.target) : 'Backup preview'" style="width: min(900px, 92vw)" :content-style="{ maxHeight: '65vh', overflowY: 'auto' }">
      <p>Backup contents can include private paths and device settings.</p>
      <p>{{ preview?.preserved ? 'Preserved state from before a restore' : 'Previous saved state' }}</p>
      <n-alert v-if="error" type="error">{{ error }}</n-alert>
      <n-alert v-if="preview?.parse_error" type="error">Invalid JSON: {{ preview.parse_error }}</n-alert>
      <n-alert v-else-if="preview?.validation_error" type="error">Backup validation failed: {{ preview.validation_error }}</n-alert>
      <n-alert v-for="(warning, index) in preview?.warnings ?? []" :key="index" type="warning">{{ warning }}</n-alert>
      <n-alert v-if="preview?.truncated" type="warning">Only the first 64 KiB are shown. This is not the complete backup.</n-alert>
      <n-input :value="preview?.json ?? ''" type="textarea" readonly :autosize="{ minRows: 10, maxRows: 22 }" />
      <template v-if="!preview?.preserved">
        <p>Restoring reloads the selected settings and may change cooling, lighting or displays. The previous file is saved as .before-restore. Review and remove an existing .before-restore file before restoring again.</p>
        <n-checkbox v-model:checked="confirmed">I have reviewed this backup and its warnings and want to replace the current state.</n-checkbox>
      </template>
      <p>Deleting removes only this backup file and permanently loses this recovery copy. Current settings and media files remain intact.</p>
      <n-checkbox v-model:checked="deleteConfirmed">I have reviewed this file and want to permanently delete it.</n-checkbox>
      <template #footer><n-space justify="end">
        <n-button @click="show = false">Close</n-button>
        <n-button type="error" :loading="busy" :disabled="!deleteConfirmed || busy || !daemon.canWrite || !daemon.info?.capabilities.includes('backup_cleanup')" @click="remove">Delete backup</n-button>
        <n-button v-if="!preview?.preserved" type="warning" :loading="busy" :disabled="!confirmed || busy || !daemon.canWrite || !daemon.info?.capabilities.includes('backup_restore') || !!preview?.parse_error || !!preview?.validation_error" @click="restore">Restore backup</n-button>
      </n-space></template>
    </n-modal>
  </n-card>
</template>
