<script setup lang="ts">
import { computed } from "vue";
import { Plus } from "lucide-vue-next";
import { useConfigStore } from "@/stores/config";
import { useDevicesStore } from "@/stores/devices";
import LcdConfigCard from "@/components/lcd/LcdConfigCard.vue";
import MediaAccessNotice from "@/components/lcd/MediaAccessNotice.vue";
import ManagedMediaImport from "@/components/lcd/ManagedMediaImport.vue";
import StartupImageDialog from "@/components/lcd/StartupImageDialog.vue";

const config = useConfigStore();
const devices = useDevicesStore();

const entries = computed(() => config.config.lcds);
const wirelessImageDevices = computed(() => devices.list.filter(device => device.family === "WirelessAio" && device.startup_image));
const selectedTemplates = computed(() => {
  const ids = new Set(entries.value.filter((entry) => entry.type === "custom").map((entry) => entry.template_id));
  return config.templates.filter((template) => ids.has(template.id));
});

function addLcd() {
  const first = devices.lcdDevices[0];
  config.addLcd({
    serial: first?.serial ?? null,
    index: first?.serial ? undefined : 0,
    type: "image",
    path: null,
    fps: null,
    orientation: 0,
    rgb: null,
  });
}
</script>

<template>
  <div class="page lcd-page">
    <div class="page-head">
      <n-button size="small" type="primary" @click="addLcd" :disabled="!devices.lcdDevices.length">
        <template #icon><Plus :size="15" /></template>
        Add LCD
      </n-button>
      <span v-if="!devices.lcdDevices.length && !wirelessImageDevices.length" class="muted">
        No LCD devices detected.
      </span>
    </div>

    <MediaAccessNotice :lcds="entries" :templates="selectedTemplates" />
    <ManagedMediaImport :lcds="entries" :templates="selectedTemplates" />

    <div v-for="device in wirelessImageDevices" :key="device.device_id" class="card page-head">
      <span>{{ device.name }} · Wireless</span>
      <StartupImageDialog :device="device" />
    </div>

    <LcdConfigCard
      v-for="(entry, i) in entries"
      :key="i"
      :entry="entry"
      :index="i"
    />

    <div v-if="!entries.length && devices.lcdDevices.length" class="card empty muted">
      No LCD configurations. Click "Add LCD" to create one.
    </div>
  </div>
</template>

<style scoped>
.lcd-page {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
  max-width: 1100px;
}
.page-head {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.empty {
  padding: var(--space-6);
  text-align: center;
}
</style>
