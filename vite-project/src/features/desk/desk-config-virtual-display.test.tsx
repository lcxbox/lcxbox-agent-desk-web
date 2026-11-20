import { cleanup, render, screen } from "@testing-library/react"
import { afterEach, describe, expect, it, vi } from "vitest"
import type { DisplayInfo, RemoteAccessInitializedData } from "@/services/types"
import { DeskConfigDialog } from "./desk-config-dialog"
import {
    DESK_CONFIG_DEFAULTS,
    resolveSavedDeviceName,
    resumeKeepsSavedChoices,
    toDeskDevicePreferences,
    VIRTUAL_DISPLAY_DEVICE,
} from "./desk-config-model"
import { DEFAULT_DESK_DEVICE_PREFERENCES, type DeskDevicePreferencesV1 } from "./desk-preferences"

// Interpolated values are visible in the rendered text.
vi.mock("react-i18next", () => ({
    useTranslation: () => ({
        t: (key: string, options?: { name?: string }) =>
            options?.name ? `${key}[${options.name}]` : key,
    }),
}))

afterEach(cleanup)

// Windows gives the virtual display the next GDI name every time the IDD is
// re-created, so a saved choice of it must survive renumbering.

const display = (name: string) => ({
    device_name: name,
    display_device_name: null,
    attached_to_desktop: true,
    rotation: 0,
    resolutions: [],
    desktop_coordinates: { left: 0, top: 0, right: 1920, bottom: 1080 },
}) as DisplayInfo

function initData(virtual: string | null): RemoteAccessInitializedData {
    const names = virtual ? [String.raw`\\.\DISPLAY1`, virtual] : [String.raw`\\.\DISPLAY1`]
    return {
        connection_epoch: `epoch-${virtual}`,
        virtual_display_device_name: virtual,
        audio_device_list: {},
        audio_encoder_list: ["Opus"],
        video_device_list: { GDI: names.map(display) },
        video_encoder_list: ["X264"],
        suggested_session_settings: {
            capture_audio: false, image_capture: "GDI", video_device_name: String.raw`\\.\DISPLAY1`,
            show_mouse: true, video_encoder: "X264", video_quality: 22, video_fps: 60,
            enable_dirty_rect: true, adaptive_bitrate: true, audio_capture: null,
            audio_device: null, audio_encoder: "Opus",
        },
        session_settings_capabilities: {
            capture_audio: "apply", image_capture: "apply", video_device_name: "apply",
            show_mouse: "apply", video_encoder: "apply", video_quality: "apply", video_fps: "apply",
            enable_dirty_rect: "apply", adaptive_bitrate: "apply", audio_capture: "apply",
            audio_device: "apply", audio_encoder: "apply",
        },
    } as unknown as RemoteAccessInitializedData
}

const savedVirtual: DeskDevicePreferencesV1 = {
    ...DEFAULT_DESK_DEVICE_PREFERENCES,
    captureAudio: false,
    imageCapture: "GDI",
    videoDeviceName: VIRTUAL_DISPLAY_DEVICE,
    videoEncoder: "X264",
}

function renderDialog(data: RemoteAccessInitializedData, onSubmit = vi.fn(), autoSubmit = false) {
    render(
        <DeskConfigDialog
            open
            onOpenChange={() => {}}
            initData={data}
            preferences={savedVirtual}
            onSubmit={onSubmit}
            onCancel={() => {}}
            autoSubmit={autoSubmit}
            onRestorePhysicalDisplay={() => {}}
            adaptiveQualityEnabled
            onAdaptiveQualityChange={() => {}}
            adaptiveBitrateEnabled
            onAdaptiveBitrateChange={() => {}}
            systemAudioAllowed
        />,
    )
    return onSubmit
}

describe("virtual display identity in device preferences", () => {
    const values = {
        ...DESK_CONFIG_DEFAULTS,
        image_capture: "GDI",
        video_encoder: "X264",
        enable_audio: false,
    }

    it("saves the virtual display as a placeholder and a physical display by name", () => {
        expect(toDeskDevicePreferences(
            { ...values, video_device_name: String.raw`\\.\DISPLAY4` },
            String.raw`\\.\DISPLAY4`,
        ).videoDeviceName).toBe(VIRTUAL_DISPLAY_DEVICE)
        expect(toDeskDevicePreferences(
            { ...values, video_device_name: String.raw`\\.\DISPLAY1` },
            String.raw`\\.\DISPLAY4`,
        ).videoDeviceName).toBe(String.raw`\\.\DISPLAY1`)
        // No virtual display on this host: names are kept as they are.
        expect(toDeskDevicePreferences(
            { ...values, video_device_name: String.raw`\\.\DISPLAY4` },
        ).videoDeviceName).toBe(String.raw`\\.\DISPLAY4`)
    })

    it("resolves the placeholder to the current virtual display", () => {
        expect(resolveSavedDeviceName(VIRTUAL_DISPLAY_DEVICE, String.raw`\\.\DISPLAY5`))
            .toBe(String.raw`\\.\DISPLAY5`)
        expect(resolveSavedDeviceName(VIRTUAL_DISPLAY_DEVICE, null)).toBe(VIRTUAL_DISPLAY_DEVICE)
        // Legacy saved GDI names are unchanged.
        expect(resolveSavedDeviceName(String.raw`\\.\DISPLAY4`, String.raw`\\.\DISPLAY5`))
            .toBe(String.raw`\\.\DISPLAY4`)
    })

    it("keeps a saved virtual display choice across renumbering when resuming", () => {
        expect(resumeKeepsSavedChoices(
            { ...values, video_device_name: String.raw`\\.\DISPLAY5` },
            savedVirtual,
            String.raw`\\.\DISPLAY5`,
        )).toBe(true)
        expect(resumeKeepsSavedChoices(
            { ...values, video_device_name: String.raw`\\.\DISPLAY1` },
            savedVirtual,
            String.raw`\\.\DISPLAY5`,
        )).toBe(false)
    })

    it("selects the re-created virtual display after it was renumbered", () => {
        const onSubmit = renderDialog(initData(String.raw`\\.\DISPLAY5`), vi.fn(), true)
        expect(onSubmit).toHaveBeenCalledTimes(1)
        const [settings, preferences] = onSubmit.mock.calls[0]
        expect(settings.video_device_name).toBe(String.raw`\\.\DISPLAY5`)
        expect(preferences.videoDeviceName).toBe(VIRTUAL_DISPLAY_DEVICE)
        expect(screen.queryByText(/displayDeviceStaleWarning/)).toBeNull()
    })

    it("names the missing virtual display in words, not by its placeholder", () => {
        renderDialog(initData(null))
        expect(screen.getByText(
            "pages.desk.displayDeviceStaleWarning.body[pages.desk.virtualDisplayBadge]",
        )).toBeInTheDocument()
        expect(screen.queryByText(new RegExp(VIRTUAL_DISPLAY_DEVICE))).toBeNull()
    })
})
