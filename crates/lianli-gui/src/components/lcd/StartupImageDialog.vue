<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, ref, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import type { DeviceInfo } from "@/types";
import { useIpc } from "@/composables/useIpc";
import { useDaemonStore } from "@/stores/daemon";

type Job = { id: number; device_id: string; status: { state: string; message?: string; response_received?: boolean } };
const props = defineProps<{ device: DeviceInfo }>();
const ipc = useIpc();
const daemon = useDaemonStore();
const jobInstance = ref<string>();
const show = ref(false);
const canvas = ref<HTMLCanvasElement | null>(null);
const image = ref<HTMLImageElement | null>(null);
const rotation = ref(0);
const zoom = ref(100);
const panX = ref(0);
const panY = ref(0);
const error = ref("");
const payload = ref("");
const submitting = ref(false);
const picking = ref(false);
const job = ref<Job | null>(null);
const capabilities = computed(() => props.device.startup_image);
const wirelessH2 = computed(() => props.device.family === "WirelessAio");
const busy = computed(() => submitting.value || ["pending", "transferring"].includes(job.value?.status.state ?? ""));
let timer: ReturnType<typeof setTimeout> | undefined;
let previewTimer: ReturnType<typeof setTimeout> | undefined;
const previewPending = ref(false);
let imageUrl: string | undefined;
let selection = 0;
let disposed = false;

function draw() {
  previewPending.value = false;
  const source = image.value;
  const target = canvas.value;
  const caps = capabilities.value;
  if (!source || !target || !caps) return;
  const output = document.createElement("canvas");
  output.width = caps.width;
  output.height = caps.height;
  const context = output.getContext("2d");
  if (!context) return;
  const quarterTurn = rotation.value % 180 !== 0;
  const rotatedPreview = quarterTurn && caps.width !== caps.height;
  const width = quarterTurn ? source.naturalHeight : source.naturalWidth;
  const height = quarterTurn ? source.naturalWidth : source.naturalHeight;
  const scale = Math.max(caps.width / width, caps.height / height) * zoom.value / 100;
  context.fillStyle = "black";
  context.fillRect(0, 0, caps.width, caps.height);
  context.save();
  const horizontal = rotatedPreview ? -panY.value : panX.value;
  const vertical = rotatedPreview ? panX.value : panY.value;
  context.translate(caps.width / 2 + horizontal / 100 * Math.abs(width * scale - caps.width) / 2,
    caps.height / 2 + vertical / 100 * Math.abs(height * scale - caps.height) / 2);
  context.rotate(rotation.value * Math.PI / 180);
  context.scale(scale, scale);
  context.drawImage(source, -source.naturalWidth / 2, -source.naturalHeight / 2);
  context.restore();
  payload.value = output.toDataURL("image/jpeg", 0.95).split(",")[1] ?? "";
  target.width = rotatedPreview ? caps.height : caps.width;
  target.height = rotatedPreview ? caps.width : caps.height;
  const preview = target.getContext("2d");
  if (preview) {
    preview.translate(target.width / 2, target.height / 2);
    if (rotatedPreview) preview.rotate(-Math.PI / 2);
    preview.drawImage(output, -caps.width / 2, -caps.height / 2);
  }
  const bytes = Math.floor(payload.value.length * 3 / 4);
  error.value = bytes > caps.max_jpeg_bytes ? `This crop exceeds the ${caps.max_jpeg_bytes.toLocaleString()}-byte limit. Choose a simpler image or crop.` : "";
}

async function chooseFile() {
  if (busy.value || picking.value) return;
  const current = ++selection;
  picking.value = true;
  try {
    const bytes = await invoke<ArrayBuffer>("pick_startup_image");
    if (current !== selection || disposed || bytes.byteLength === 0) return;
    payload.value = "";
    image.value = null;
    error.value = "";
    if (imageUrl) URL.revokeObjectURL(imageUrl);
    imageUrl = URL.createObjectURL(new Blob([bytes]));
    const source = new Image();
    source.src = imageUrl;
    await source.decode();
    if (current !== selection || disposed) return;
    if (source.naturalWidth * source.naturalHeight > 16_777_216) throw new Error("Image exceeds 16 megapixels.");
    image.value = source;
    rotation.value = 0; zoom.value = 100; panX.value = 0; panY.value = 0;
    await nextTick();
    draw();
  } catch (e) { if (current === selection) error.value = String(e); }
  finally { picking.value = false; }
}

async function poll() {
  const instance = jobInstance.value;
  try {
    const current = await ipc.request<Job | null>("GetStartupImageStatus");
    if (disposed || !show.value || instance !== daemon.info?.instance_id) return;
    if (current?.device_id === props.device.device_id) job.value = current;
    else if (job.value && busy.value) { job.value = null; error.value = "Upload status changed. Check the device before attempting another upload."; return; }
    if (show.value && (current?.status.state === "pending" || current?.status.state === "transferring")) timer = setTimeout(poll, 800);
  } catch (e) {
    if (disposed || !show.value) return;
    error.value = `Could not read upload status: ${String(e)}`;
    if (busy.value && instance === daemon.info?.instance_id) timer = setTimeout(poll, 2000);
  }
}

async function upload() {
  if (busy.value || previewPending.value || error.value || !payload.value) return;
  if (!daemon.info?.instance_id) { error.value = "Refresh the daemon connection before uploading."; return; }
  jobInstance.value = daemon.info.instance_id;
  const instance = jobInstance.value;
  submitting.value = true;
  try {
    const { id } = await ipc.request<{ id: number }>("UploadStartupImage", { device_id: props.device.device_id, jpeg_base64: payload.value }, jobInstance.value);
    if (disposed || instance !== daemon.info?.instance_id) return;
    job.value = { id, device_id: props.device.device_id, status: { state: "pending" } };
    await poll();
  } catch (e) { error.value = String(e); }
  finally { submitting.value = false; }
}

async function cancel() {
  if (!job.value) return;
  try { await ipc.request("CancelStartupImage", { id: job.value.id }, jobInstance.value); }
  catch (e) { error.value = String(e); }
}

watch([rotation, zoom, panX, panY], () => { clearTimeout(previewTimer); previewPending.value = true; previewTimer = setTimeout(draw, 120); });
watch(show, async (visible) => {
  clearTimeout(timer);
  if (visible) { jobInstance.value = daemon.info?.instance_id; await nextTick(); draw(); await poll(); }
  else {
    selection++; clearTimeout(previewTimer); image.value = null; payload.value = "";
    if (canvas.value) { canvas.value.width = 0; canvas.value.height = 0; }
    if (imageUrl) { URL.revokeObjectURL(imageUrl); imageUrl = undefined; }
  }
});
watch(() => daemon.info?.instance_id, (instance) => {
  if (jobInstance.value && instance !== jobInstance.value && busy.value) {
    clearTimeout(timer); job.value = null; error.value = "Daemon disconnected. Check the startup image before retrying.";
  }
});
onBeforeUnmount(() => { disposed = true; selection++; clearTimeout(timer); clearTimeout(previewTimer); if (imageUrl) URL.revokeObjectURL(imageUrl); });
</script>

<template>
  <n-button v-if="capabilities" size="small" @click="show = true">Startup Image</n-button>
  <n-modal v-model:show="show" preset="card" title="Startup Image" style="width: min(760px, 95vw)" :mask-closable="!busy" :closable="!busy" :close-on-esc="!busy">
    <div class="startup-editor">
      <p><strong>{{ device.name }}</strong> · {{ capabilities?.width }} × {{ capabilities?.height }}</p>
      <p v-if="wirelessH2" class="hint">Experimental H2 wireless upload. USB playback pauses during transfer and resumes afterward. Boot persistence is not verified.</p>
      <p v-else class="hint">Save an image to show at startup. Playback pauses during upload.</p>
      <p v-if="capabilities?.jpeg_target_bytes" class="hint">JPEG quality is adjusted automatically to fit the panel's {{ capabilities.jpeg_target_bytes.toLocaleString() }}-byte image budget.</p>
      <n-button :disabled="busy || picking" :loading="picking" @click="chooseFile">Choose image…</n-button>
      <div class="preview"><canvas ref="canvas" v-show="image" aria-label="Native-size startup image crop preview" /></div>
      <template v-if="image">
        <label>Rotation <n-select v-model:value="rotation" :disabled="busy" :options="[0, 90, 180, 270].map(value => ({ label: `${value}°`, value }))" /></label>
        <label>Zoom ({{ zoom }}%) <n-slider v-model:value="zoom" :min="1" :max="300" :disabled="busy" /></label>
        <label>Horizontal position <n-slider v-model:value="panX" :min="-100" :max="100" :disabled="busy" /></label>
        <label>Vertical position <n-slider v-model:value="panY" :min="-100" :max="100" :disabled="busy" /></label>
      </template>
      <n-alert v-if="error" type="error">{{ error }}</n-alert>
      <n-alert v-if="job?.status.state === 'failed'" type="error">{{ job.status.message }}</n-alert>
      <n-alert v-else-if="job?.status.state === 'transferred'" type="info"><template v-if="wirelessH2">Wireless transfer acknowledged. Saving and boot display still need hardware verification.</template><template v-else>Image transferred. {{ job.status.response_received ? '' : 'The screen did not confirm the upload. ' }}Power-cycle the screen to check the startup image.</template></n-alert>
      <p v-else-if="job?.status.state === 'cancelled'">{{ wirelessH2 ? 'Upload cancelled. An image already submitted may still be saved.' : 'Cancelled before transfer.' }}</p>
      <p v-if="busy">{{ job?.status.state === 'transferring' ? 'Uploading. Keep the screen connected.' : 'Preparing upload…' }}</p>
      <div class="actions">
        <n-button v-if="busy" @click="cancel">Request cancellation</n-button>
        <n-button type="primary" :loading="busy" :disabled="busy || picking || previewPending || !payload || !!error" @click="upload">Upload startup image</n-button>
      </div>
    </div>
  </n-modal>
</template>

<style scoped>
.startup-editor { display: flex; flex-direction: column; gap: var(--space-3); }
.startup-editor p { margin: 0; }
.preview { display: flex; justify-content: center; background: #111; }
.preview canvas { max-width: 100%; max-height: 320px; object-fit: contain; }
.startup-editor label { display: grid; grid-template-columns: 130px 1fr; align-items: center; gap: var(--space-3); }
.actions { display: flex; justify-content: flex-end; gap: var(--space-2); }
</style>
