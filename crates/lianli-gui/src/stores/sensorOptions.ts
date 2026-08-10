import type { SensorInfo, SensorSourceConfig } from "@/types";

export interface SelectOption {
  label: string;
  value: string;
}

/**
 * Build NSelect options from the enumerated sensor list.
 *
 * When `includeCommand` is true, a "Custom command" sentinel option is
 * appended (value "command") so fan curves can fall back to a shell command.
 *
 * When `tempOnly` is true, only temperature sensors (unit "C") are included —
 * use this for fan curve temperature sources where %, RPM, etc. are invalid.
 */
export function enumerateSensorsAsOptions(
  sensors: SensorInfo[],
  includeCommand: boolean,
  tempOnly: boolean = false,
): SelectOption[] {
  const filtered = tempOnly ? sensors.filter((s) => s.unit === "C") : sensors;
  const opts: SelectOption[] = filtered.map((s) => ({
    label: formatSensorLabel(s),
    value: JSON.stringify(s.source),
  }));
  if (includeCommand) {
    opts.push({ label: "Custom command", value: "command" });
  }
  return opts;
}

/** Build options from SensorSourceConfig[] (e.g. for AIO source dropdowns). */
export function sourceConfigsAsOptions(
  sensors: SensorInfo[],
): SelectOption[] {
  return sensors.map((s) => ({
    label: formatSensorLabel(s),
    value: JSON.stringify(s.source),
  }));
}

function formatSensorLabel(s: SensorInfo): string {
  const unit = UNIT_LABELS[s.unit];
  if (s.display_name) {
    return unit ? `${s.display_name} (${unit})` : s.display_name;
  }
  const sensor = s.sensor_name?.sensor_name ?? "sensor";
  const device = s.sensor_name?.device_name;
  let name = device && device !== sensor ? `${device}: ${sensor}` : sensor;
  if (s.source?.type === "hwmon" && s.source.name && !name.includes(s.source.name)) {
    name = `${name} [${s.source.name}]`;
  }
  return unit ? `${name} (${unit})` : name;
}

const UNIT_LABELS: Record<string, string> = {
  C: "\u00b0C",
  RPM: "RPM",
  V: "mV",
  FREQ: "MHz",
  PERCENT: "%",
  SIZE: "GB",
  MBps: "MB/s",
  WO: "",
};

/** Decode a selected option value back into a SensorSourceConfig. */
export function decodeOption(value: string): SensorSourceConfig | null {
  if (!value || value === "command") return null;
  try {
    return JSON.parse(value) as SensorSourceConfig;
  } catch {
    return null;
  }
}

/** Find the option value matching a stored config (for initial selection). */
export function optionForConfig(
  sensors: SensorInfo[],
  cfg: SensorSourceConfig | null | undefined,
): string {
  if (!cfg) return "";
  const json = JSON.stringify(cfg);
  return sensors.some((s) => JSON.stringify(s.source) === json) ? json : "";
}
