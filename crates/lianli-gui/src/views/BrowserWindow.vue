<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import { RefreshCw, Download, CheckCircle, AlertCircle, Loader2, X, ExternalLink } from "lucide-vue-next";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { open as openUrl } from "@tauri-apps/plugin-shell";
import { getVersion } from "@tauri-apps/api/app";
import type { CatalogManifest, CatalogTemplate } from "@/types";
import { useConfigStore } from "@/stores/config";
import { useLcdStore, LCD_TEMPLATES_CHANGED_EVENT, type CatalogInstallStatus } from "@/stores/lcd";
import { emit } from "@tauri-apps/api/event";
import { boundedFetch } from "@/utils/boundedFetch";

const config = useConfigStore();
const lcd = useLcdStore();

const CATALOG_URL =
  "https://raw.githubusercontent.com/sgtaziz/lian-li-linux/main/templates/default_templates.json";
const ASSET_BASE =
  "https://raw.githubusercontent.com/sgtaziz/lian-li-linux/main/templates/assets";

const loading = ref(false);
const error = ref("");
const templates = ref<CatalogTemplate[]>([]);
const previewCache = ref<Record<string, string>>({});
const previewErrors = ref<Record<string, string>>({});
let downloads = new AbortController();
let disposed = false;
function clearPreviews() {
  for (const url of Object.values(previewCache.value)) URL.revokeObjectURL(url);
  previewCache.value = {};
  previewErrors.value = {};
}
onUnmounted(() => { disposed = true; downloads.abort(); clearPreviews(); });
const installState = ref<Record<string, "idle" | "installing" | "installed" | "error">>({});

const installedIds = ref<Set<string>>(new Set());
const monitoringInstall = ref(false);
const startingInstall = ref(false);
const installProgress = ref("");

onMounted(async () => {
  await config.load().catch(() => undefined);
  for (const t of config.templates) installedIds.value.add(t.id);
  if (!disposed) void checkInstall();
  await fetchCatalog();
});

async function fetchCatalog() {
  if (loading.value || disposed) return;
  downloads.abort();
  downloads = new AbortController();
  const controller = downloads;
  clearPreviews();
  loading.value = true;
  error.value = "";
  try {
    const blob = await boundedFetch(CATALOG_URL, 1024 * 1024, controller.signal);
    const manifest: CatalogManifest = JSON.parse(await blob.text());
    if (manifest.schema_version !== 1) {
      throw new Error(`unsupported catalog schema version ${manifest.schema_version}`);
    }
    if (!Array.isArray(manifest.templates) || manifest.templates.length > 128
      || manifest.templates.some((t) => !t || [t.id, t.name, t.folder, t.preview, t.min_daemon_version].some((value) => typeof value !== "string" || value.length > 512))
      || new Set(manifest.templates.map((t) => t.id)).size !== manifest.templates.length) {
      throw new Error("Catalog must contain at most 128 templates with unique IDs and valid metadata");
    }
    const ver = await getVersion().catch(() => null);
    if (controller.signal.aborted) return;
    templates.value = ver
      ? manifest.templates.filter((t) => versionGte(ver, t.min_daemon_version))
      : manifest.templates;
    void loadPreviews(templates.value, controller);
  } catch (e) {
    if (!controller.signal.aborted) error.value = `${e}. Use Refresh to retry.`;
  } finally {
    loading.value = false;
  }
}

function versionGte(have: string, need: string): boolean {
  const parse = (s: string) =>
    s
      .replace(/^v/, "")
      .split(".")
      .map((p) => parseInt(p.replace(/[^0-9].*/, ""), 10) || 0);
  const [hh = 0, hm = 0, hp = 0] = parse(have);
  const [nh = 0, nm = 0, np = 0] = parse(need);
  return hh > nh || (hh === nh && (hm > nm || (hm === nm && hp >= np)));
}

async function loadPreviews(items: CatalogTemplate[], controller: AbortController) {
  let next = 0;
  let cachedBytes = 0;
  await Promise.all(Array.from({ length: 4 }, async () => {
    while (!controller.signal.aborted && next < items.length) {
      const t = items[next++];
      try {
        if (cachedBytes >= 16 * 1024 * 1024) throw new Error("Preview cache limit reached");
        const blob = await boundedFetch(`${ASSET_BASE}/${t.folder}/${t.preview}`, 1024 * 1024, controller.signal);
        if (controller.signal.aborted) return;
        if (cachedBytes + blob.size > 16 * 1024 * 1024) throw new Error("Preview cache limit reached");
        cachedBytes += blob.size;
        previewCache.value[t.id] = URL.createObjectURL(blob);
      } catch (e) {
        if (!controller.signal.aborted) previewErrors.value[t.id] = `${e}. Use Refresh to retry previews.`;
      }
    }
  }));
}

async function install(t: CatalogTemplate) {
  if (startingInstall.value || monitoringInstall.value || installState.value[t.id] === "installing") return;
  startingInstall.value = true;
  installState.value[t.id] = "installing";
  try {
    await monitorInstall(await lcd.installTemplate(t));
  } catch (e) {
    installState.value[t.id] = "error";
    error.value = `Installation could not be confirmed: ${e}. Use Check install before retrying; daemon work may still finish.`;
  } finally {
    startingInstall.value = false;
  }
}

async function checkInstall() {
  if (monitoringInstall.value || disposed) return;
  try {
    const status = await lcd.catalogInstallStatus();
    if (status) await monitorInstall(status);
    else installProgress.value = "No catalog installation recorded by this daemon.";
  } catch (e) {
    installProgress.value = `Install status unavailable: ${e}. Use Check install to retry.`;
  }
}

async function monitorInstall(initial: CatalogInstallStatus) {
  if (monitoringInstall.value || disposed) return;
  monitoringInstall.value = true;
  let status = initial;
  try {
    while (!disposed) {
      installState.value[status.template_id] = status.finished
        ? (status.error ? "error" : "installed") : "installing";
      if (status.finished) {
        if (status.error) throw new Error(status.error);
        installProgress.value = `Saved ${status.template_id}.`;
        await emit(LCD_TEMPLATES_CHANGED_EVENT);
        await config.load();
        installedIds.value = new Set(config.templates.map((template) => template.id));
        return;
      }
      installProgress.value = `Installing ${status.template_id}… Closing this window leaves daemon installation running.`;
      await new Promise((resolve) => setTimeout(resolve, 1000));
      if (disposed) return;
      const next = await lcd.catalogInstallStatus();
      if (!next || next.operation_id !== initial.operation_id) {
        throw new Error("Daemon or installation changed. Reload templates and check the latest install status");
      }
      status = next;
    }
  } catch (e) {
    installState.value[initial.template_id] = "error";
    installProgress.value = `Installation could not be confirmed: ${e}. Use Check install before retrying.`;
  } finally {
    monitoringInstall.value = false;
  }
}

function closeWindow() {
  void getCurrentWebviewWindow().close();
}

const PUBLISHING_URL = "https://github.com/sgtaziz/lian-li-linux/tree/main/templates";

/** Effective (post-rotation) aspect ratio for a catalog template. */
function previewAspect(t: CatalogTemplate): string {
  const w = t.rotated ? t.base_height : t.base_width;
  const h = t.rotated ? t.base_width : t.base_height;
  return w && h ? `${w} / ${h}` : "1 / 1";
}
</script>

<template>
  <div class="browser-window">
    <div class="topbar">
      <span class="title">Template Browser</span>
      <button class="guide" title="Open publishing guide in your browser" @click="openUrl(PUBLISHING_URL)">
        <ExternalLink :size="13" /> Publishing Guide
      </button>
      <div class="spacer" />
      <n-button size="small" quaternary :disabled="monitoringInstall" @click="checkInstall">Check install</n-button>
      <n-button size="small" quaternary :loading="loading" @click="fetchCatalog">
        <template #icon><RefreshCw :size="14" /></template>
        Refresh
      </n-button>
      <n-button size="small" quaternary @click="closeWindow"><template #icon><X :size="14" /></template>Close</n-button>
    </div>

    <div class="content">
      <p v-if="installProgress" role="status">{{ installProgress }}</p>
      <div v-if="loading" class="state">
        <Loader2 :size="28" class="spin" />
        <span>Loading catalog…</span>
      </div>

      <div v-else-if="error" class="state error">
        <AlertCircle :size="28" />
        <span>{{ error }}</span>
      </div>

      <div v-else-if="!templates.length" class="state muted">
        No templates available.
      </div>

      <div v-else class="grid">
        <div v-for="t in templates" :key="t.id" class="card tpl-card">
          <div class="preview" :style="{ aspectRatio: previewAspect(t) }">
            <img v-if="previewCache[t.id]" :src="previewCache[t.id]" alt="" />
            <div v-else class="preview-ph" :title="previewErrors[t.id]">{{ previewErrors[t.id] ? 'Preview unavailable — Refresh to retry' : '' }}</div>
          </div>
          <div class="info">
            <div class="name">{{ t.name }}</div>
            <div class="author muted" v-if="t.author">by {{ t.author }}</div>
            <div class="badges">
              <span class="badge" v-if="t.base_width">{{ t.base_width }}×{{ t.base_height }}</span>
              <span class="badge" v-if="t.rotated">Rotated</span>
            </div>
            <div class="desc muted">{{ t.description }}</div>
            <n-button
              size="small"
              type="primary"
              :disabled="installedIds.has(t.id) || startingInstall || monitoringInstall"
              :loading="installState[t.id] === 'installing'"
              @click="install(t)"
            >
              <template v-if="installState[t.id] === 'installing'" #icon><Loader2 :size="14" class="spin" /></template>
              <template v-else-if="installState[t.id] === 'installed' || installedIds.has(t.id)" #icon><CheckCircle :size="14" /></template>
              <template v-else #icon><Download :size="14" /></template>
              {{ installedIds.has(t.id) ? "Installed" : "Install" }}
            </n-button>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.browser-window {
  display: flex;
  flex-direction: column;
  height: 100vh;
  width: 100vw;
}
.topbar {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-2) var(--space-4);
  border-bottom: 1px solid var(--border);
  background: var(--bg-surface);
}
.title {
  font-weight: 600;
}
.guide {
  font-size: var(--font-size-sm);
  background: none;
  border: none;
  color: var(--accent);
  cursor: pointer;
  padding: 0;
  display: inline-flex;
  align-items: center;
  gap: var(--space-1);
}
.guide:hover {
  color: var(--accent-hover);
}
.spacer {
  flex: 1;
}
.content {
  flex: 1;
  overflow-y: auto;
  padding: var(--space-4);
}
.state {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: var(--space-2);
  padding: var(--space-8);
  color: var(--text-muted);
}
.state.error {
  color: var(--danger);
}
.grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
  gap: var(--space-4);
}
.tpl-card {
  padding: var(--space-3);
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.preview {
  width: 100%;
  max-height: 220px;
  /* aspect-ratio is set inline from each template's effective dimensions. */
  background: #14171f;
  border-radius: var(--radius-md);
  overflow: hidden;
  border: 1px solid var(--border);
}
.preview img {
  width: 100%;
  height: 100%;
  object-fit: contain;
}
.preview-ph {
  width: 100%;
  height: 100%;
}
.name {
  font-weight: 600;
  font-size: var(--font-size-sm);
}
.author {
  font-size: var(--font-size-xs);
}
.badges {
  display: flex;
  gap: var(--space-1);
  flex-wrap: wrap;
}
.badge {
  font-size: var(--font-size-xs);
  background: var(--bg-elevated);
  padding: 1px var(--space-2);
  border-radius: 999px;
  color: var(--text-secondary);
}
.desc {
  font-size: var(--font-size-xs);
  min-height: 28px;
}
.spin {
  animation: spin 1s linear infinite;
}
@keyframes spin {
  to {
    transform: rotate(360deg);
  }
}
</style>
