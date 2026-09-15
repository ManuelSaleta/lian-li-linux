<script setup lang="ts">
import { useDaemonStore } from "@/stores/daemon";

const daemon = useDaemonStore();
</script>

<template>
  <n-alert v-if="daemon.connected && daemon.writeError" type="warning" title="Changes disabled">
    {{ daemon.writeError }}
  </n-alert>
</template>
