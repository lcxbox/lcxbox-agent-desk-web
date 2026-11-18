import type { TFunction } from "i18next"
import { deskErrorCodeEnum } from "@/services/types"
import { deskErrorMessage, type ErrorCodeKeyMap } from "@/lib/desk-error-i18n"

const REFUSAL_KEYS: ErrorCodeKeyMap = {
    [deskErrorCodeEnum.WAYLAND_PORTAL_AUTHORIZATION_REQUIRED]:
        "pages.desk.waylandAuthorizationRequiredDescription",
    [deskErrorCodeEnum.PERMISSION_ERROR]: "pages.desk.remoteAccessDenied",
}

/**
 * What to show in place of the desktop when the host refused to open a
 * remote session. The host answers the admission with this reason; the
 * page keeps it on screen until the next attempt.
 */
export function remoteAccessRefusalMessage(
    t: TFunction,
    code: number,
    message: string | null | undefined,
): string {
    return deskErrorMessage(t, REFUSAL_KEYS, code, message, t("pages.desk.remoteAccessFailed"))
}
