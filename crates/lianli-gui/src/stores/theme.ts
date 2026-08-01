import { defineStore } from "pinia";
import { computed, ref, watch } from "vue";

export type ThemeMode = "dark" | "light";

const STORAGE_KEY = "lianli.theme";
const VALID: ThemeMode[] = ["dark", "light"];

function readStored(): ThemeMode | null {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    return v && VALID.includes(v as ThemeMode) ? (v as ThemeMode) : null;
  } catch {
    return null;
  }
}

function prefersLight(): boolean {
  try {
    return window.matchMedia?.("(prefers-color-scheme: light)").matches ?? false;
  } catch {
    return false;
  }
}

/**
 * UI colour-scheme preference. Defaults to the OS setting, persists any
 * explicit user choice to localStorage, and reflects the active mode onto
 * `document.documentElement` so the CSS variable palette can switch.
 */
export const useThemeStore = defineStore("theme", () => {
  const stored = readStored();
  const mode = ref<ThemeMode>(stored ?? (prefersLight() ? "light" : "dark"));

  const isDark = computed(() => mode.value === "dark");

  function set(next: ThemeMode) {
    mode.value = next;
  }

  function toggle() {
    mode.value = mode.value === "dark" ? "light" : "dark";
  }

  function apply() {
    const el = document.documentElement;
    el.setAttribute("data-theme", mode.value);
    el.style.colorScheme = mode.value;
  }

  watch(
    mode,
    (next) => {
      try {
        localStorage.setItem(STORAGE_KEY, next);
      } catch {
        // storage unavailable — keep in-memory only
      }
      apply();
    },
    { immediate: true },
  );

  return { mode, isDark, set, toggle };
});
