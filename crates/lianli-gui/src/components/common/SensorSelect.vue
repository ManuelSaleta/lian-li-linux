<script setup lang="ts">
import { h } from "vue";
import type { SelectOption } from "@/stores/sensorOptions";

defineOptions({ inheritAttrs: false });

defineProps<{ options: SelectOption[] }>();

// Full label as a native tooltip so overflow stays identifiable
function renderLabel(option: { label?: unknown }) {
  const label = typeof option.label === "string" ? option.label : "";
  return h("span", { class: "sensor-select-label", title: label }, label);
}
</script>

<template>
  <n-select
    v-bind="$attrs"
    :options="options"
    filterable
    :consistent-menu-width="false"
    :menu-props="{ class: 'sensor-select-menu' }"
    :render-label="renderLabel"
  />
</template>

<style scoped>
/* Selected value scrolls horizontally instead of truncating */
:deep(.n-base-selection-overlay) {
  pointer-events: auto;
}
:deep(.n-base-selection-overlay__wrapper),
:deep(.n-base-selection-input__content) {
  overflow-x: auto;
  text-overflow: clip;
  scrollbar-width: thin;
}
</style>
