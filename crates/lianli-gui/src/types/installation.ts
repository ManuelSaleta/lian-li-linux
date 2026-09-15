export type CheckState = "passed" | "failed" | "unavailable" | "not_applicable";
export type InstallationGuide = "usb_permissions" | "service_modes" | "distrobox" | "troubleshooting";

export interface InstallationFinding {
  code: string;
  state: CheckState;
  severity: "info" | "warning" | "error";
  feature: string;
  context: string;
  title: string;
  evidence: string;
  remediation: string;
  guide: InstallationGuide;
}

export interface InstallationReport {
  context: { kind: "native" } | { kind: "distrobox"; name: string } | { kind: "unsupported_container" };
  daemon_context?: InstallationReport["context"] | null;
  checked_at_unix_ms: number;
  findings: InstallationFinding[];
  services?: ServiceReport | null;
}

export type ServiceProbe<T> = { state: "known"; value: T } | { state: "unavailable"; reason: string };

export interface UnitState {
  name: string;
  load_state: string;
  active_state: string;
  sub_state: string;
  unit_file_state: string;
  main_pid: number;
  fragment_path: string;
  control_group?: string | null;
  kill_mode?: string | null;
  send_sigkill?: boolean | null;
  graceful_shutdown?: boolean | null;
  invocation_id?: string | null;
  distrobox_name?: string | null;
}

export type ServiceScope = "user" | "system";
export type ServiceChangeRequest =
  | { kind: "switch"; scope: ServiceScope; carry_settings: boolean }
  | { kind: "recover" };
export type ServiceAction = "start" | "stop" | "restart";
export interface ServiceOperationStatus {
  active: boolean;
  message: string;
  success: boolean | null;
}

export interface ServiceReport {
  context: InstallationReport["context"];
  user: ServiceProbe<UnitState>;
  system: ServiceProbe<UnitState>;
  global_user: ServiceProbe<string>;
  operation_lock?: ServiceProbe<{ device: string; inode: string }> | null;
  selection?: ServiceProbe<{ scope: ServiceScope; uid: number } | null> | null;
  ownership?: ServiceProbe<{
    identity: { device: string; inode: string };
    owner_pid: number | null;
    process?: ServiceProbe<{
      pid: number;
      effective_uid: number;
      start_time_ticks: string;
      control_group: string;
      service: "user" | "system" | null;
    }> | null;
  }> | null;
}

export const INSTALLATION_GUIDES: Record<InstallationGuide, string> = {
  usb_permissions: "https://github.com/sgtaziz/lian-li-linux/blob/main/docs/usb-permissions.md",
  service_modes: "https://github.com/sgtaziz/lian-li-linux/blob/main/docs/service-modes.md",
  distrobox: "https://github.com/sgtaziz/lian-li-linux/blob/main/docs/distrobox.md",
  troubleshooting: "https://github.com/sgtaziz/lian-li-linux/blob/main/docs/troubleshooting.md",
};
