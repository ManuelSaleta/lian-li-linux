<script setup lang="ts">
import { computed, onMounted } from "vue";
import { useRouter } from "vue-router";
import { open } from "@tauri-apps/plugin-shell";
import { useMessage } from "naive-ui";
import { useInstallationStore } from "@/stores/installation";
import { useDaemonStore } from "@/stores/daemon";
import { INSTALLATION_GUIDES } from "@/types/installation";
import { canSetUpServices } from "@/utils/serviceSetup";
import ServiceStatus from "./ServiceStatus.vue";

const installation = useInstallationStore();
const daemon = useDaemonStore();
const router = useRouter();
const message = useMessage();
const initialSetup = computed(() => !daemon.connected && canSetUpServices(installation.report?.services));
onMounted(() => { void installation.start(); });

async function openGuide() {
  try {
    const fallback = installation.report?.context.kind === "distrobox" ? "distrobox" : "service_modes";
    await open(INSTALLATION_GUIDES[installation.issues[0]?.guide ?? fallback]);
  } catch (error) {
    message.error(`Could not open the guide: ${String(error)}`);
  }
}

function review() {
  installation.popupOpen = false;
  void router.push("/installation");
}

async function recheck() {
  await Promise.all([installation.recheck(), daemon.refresh()]);
}
</script>

<template>
  <n-modal v-model:show="installation.popupOpen" preset="card" title="Check your installation"
    style="width: min(580px, 92vw)" :content-style="{ maxHeight: '65vh', overflowY: 'auto' }">
    <p>Some setup checks need attention. You can review these at any time in Installation Health.</p>
    <n-alert v-if="installation.error" type="warning">{{ installation.error }}</n-alert>
    <ServiceStatus v-if="initialSetup" setup-only />
    <n-alert v-else-if="!daemon.connected" type="warning" title="Waiting for daemon">
      Open Settings to manage services. If the daemon is starting, wait and Recheck.
    </n-alert>
    <div v-for="finding in installation.issues" :key="finding.code" class="notice-finding">
      <strong>{{ finding.title }}</strong>
      <p>{{ finding.evidence }}</p>
      <p>{{ finding.remediation }}</p>
    </div>
    <p v-if="daemon.connected && !installation.error && !installation.issues.length">These installation checks now pass.</p>
    <template #footer>
      <n-space justify="end" wrap>
        <n-button @click="installation.popupOpen = false">Later</n-button>
        <n-button :loading="installation.checking" @click="recheck">Recheck</n-button>
        <n-button @click="openGuide">Open guide</n-button>
        <n-button type="primary" @click="review">Review checks</n-button>
      </n-space>
    </template>
  </n-modal>
</template>

<style scoped>
.notice-finding { margin-top: var(--space-4); overflow-wrap: anywhere; }
p { margin: var(--space-2) 0; }
</style>
