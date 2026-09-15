import type { ServiceReport } from "@/types/installation";

export function canSwitchServices(report?: ServiceReport | null): boolean {
  if (report?.context.kind === "native") return true;
  if (report?.context.kind !== "distrobox") return false;
  const name = report.context.name;
  return [report.user, report.system].every((unit) => unit.state === "known"
    && unit.value.load_state === "loaded" && unit.value.distrobox_name === name);
}

export function canSetUpServices(report?: ServiceReport | null): boolean {
  if (!report || !canSwitchServices(report)
    || report.operation_lock?.state !== "known"
    || report.selection?.state !== "known" || report.selection.value !== null
    || report.ownership?.state !== "known" || report.ownership.value.owner_pid !== null
    || report.global_user.state !== "known" || report.global_user.value !== "disabled") return false;
  return [report.user, report.system].every((unit) => unit.state === "known"
    && unit.value.load_state === "loaded" && unit.value.active_state === "inactive"
    && unit.value.main_pid === 0 && unit.value.unit_file_state === "disabled");
}
