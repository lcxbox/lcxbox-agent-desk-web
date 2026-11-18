import { describe, expect, it } from "vitest"
import type { TFunction } from "i18next"
import { deskErrorCodeEnum } from "@/services/types"
import { remoteAccessRefusalMessage } from "./remote-access-refusal"

const t = ((key: string) => key) as unknown as TFunction

describe("remoteAccessRefusalMessage", () => {
    it("explains a Wayland host that still needs local authorization", () => {
        expect(remoteAccessRefusalMessage(
            t,
            deskErrorCodeEnum.WAYLAND_PORTAL_AUTHORIZATION_REQUIRED,
            "Wayland remote access must be enabled on the host before connecting",
        )).toBe("pages.desk.waylandAuthorizationRequiredDescription")
    })

    it("explains a refused session", () => {
        expect(remoteAccessRefusalMessage(t, deskErrorCodeEnum.PERMISSION_ERROR, "denied"))
            .toBe("pages.desk.remoteAccessDenied")
    })

    it("falls back to the host's reason, then to a generic one", () => {
        expect(remoteAccessRefusalMessage(t, deskErrorCodeEnum.INVALID_STATE, "connection was disconnected"))
            .toBe("connection was disconnected")
        expect(remoteAccessRefusalMessage(t, deskErrorCodeEnum.SYSTEM_ERROR, undefined))
            .toBe("pages.desk.remoteAccessFailed")
    })
})
