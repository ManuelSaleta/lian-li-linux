import type { ServiceReport } from "@/types/installation";

export function canSetUpServices(report?: ServiceReport | null): boolean {
  if (report?.context.kind !== "native"
    || report.operation_lock?.state !== "known"
    || report.selection?.state !== "known" || report.selection.value !== null
    || report.ownership?.state !== "known" || report.ownership.value.owner_pid !== null
    || report.global_user.state !== "known" || report.global_user.value !== "disabled") return false;
  return [report.user, report.system].every((unit) => unit.state === "known"
    && unit.value.load_state === "loaded" && unit.value.active_state === "inactive"
    && unit.value.main_pid === 0 && unit.value.unit_file_state === "disabled");
}
