import { cleanup, render } from "@testing-library/react"
import { afterEach, describe, expect, it, vi } from "vitest"
import type { DisplayInfo, RemoteAccessInitializedData } from "@/services/types"
import { DeskConfigDialog } from "./desk-config-dialog"
import { DESK_CONFIG_DEFAULTS, resumeKeepsSavedChoices } from "./desk-config-model"
import { DEFAULT_DESK_DEVICE_PREFERENCES, type DeskDevicePreferencesV1 } from "./desk-preferences"
import { createSessionResumeSlot } from "./session-resume"

afterEach(cleanup)

const display = (name: string) => ({
    device_name: name,
    display_device_name: null,
    attached_to_desktop: true,
    rotation: 0,
    resolutions: [],
    desktop_coordinates: { left: 0, top: 0, right: 1920, bottom: 1080 },
}) as DisplayInfo

function makeInitData(epoch = "e2", displays = ["primary", "secondary"]): RemoteAccessInitializedData {
    return {
        connection_epoch: epoch,
        audio_device_list: {},
        audio_encoder_list: ["Opus"],
        video_device_list: { DXGI: displays.map(display), WGC: displays.map(display) },
        video_encoder_list: ["X264", "H264"],
        suggested_session_settings: {
            capture_audio: false,
            image_capture: "WGC",
            video_device_name: "primary",
            show_mouse: true,
            video_encoder: "X264",
            video_quality: 22,
            video_fps: 60,
            enable_dirty_rect: true,
            adaptive_bitrate: true,
            audio_capture: null,
            audio_device: null,
            audio_encoder: "Opus",
        },
        session_settings_capabilities: {
            capture_audio: "apply",
            image_capture: "apply",
            video_device_name: "apply",
            show_mouse: "apply",
            video_encoder: "apply",
            video_quality: "apply",
            video_fps: "apply",
            enable_dirty_rect: "apply",
            adaptive_bitrate: "apply",
            audio_capture: "apply",
            audio_device: "apply",
            audio_encoder: "apply",
        },
    } as unknown as RemoteAccessInitializedData
}

/** The user last connected over DXGI to the secondary display with OpenH264. */
const saved: DeskDevicePreferencesV1 = {
    ...DEFAULT_DESK_DEVICE_PREFERENCES,
    captureAudio: false,
    imageCapture: "DXGI",
    videoDeviceName: "secondary",
    videoEncoder: "H264",
    videoQuality: 30,
}

function renderDialog(props: {
    initData?: RemoteAccessInitializedData
    preferences: DeskDevicePreferencesV1 | null
    autoSubmit: boolean
    onSubmit?: ReturnType<typeof vi.fn>
    onAutoSubmitDeclined?: ReturnType<typeof vi.fn>
}) {
    const onSubmit = props.onSubmit ?? vi.fn()
    const onAutoSubmitDeclined = props.onAutoSubmitDeclined ?? vi.fn()
    const element = (p: typeof props) => (
        <DeskConfigDialog
            open
            onOpenChange={() => {}}
            initData={p.initData ?? makeInitData()}
            preferences={p.preferences}
            onSubmit={onSubmit}
            onCancel={() => {}}
            autoSubmit={p.autoSubmit}
            onAutoSubmitDeclined={onAutoSubmitDeclined}
            onRestorePhysicalDisplay={() => {}}
            adaptiveQualityEnabled
            onAdaptiveQualityChange={() => {}}
            adaptiveBitrateEnabled
            onAdaptiveBitrateChange={() => {}}
            systemAudioAllowed
        />
    )
    const view = render(element(props))
    return {
        onSubmit,
        onAutoSubmitDeclined,
        rerender: (next: typeof props) => view.rerender(element(next)),
    }
}

describe("config dialog auto-resume", () => {
    it("submits the saved choices once, marked as a resume", () => {
        const { onSubmit, rerender } = renderDialog({ preferences: saved, autoSubmit: true })
        expect(onSubmit).toHaveBeenCalledTimes(1)
        const [settings, , source] = onSubmit.mock.calls[0]
        expect(source).toBe("resume")
        expect(settings.image_capture).toBe("DXGI")
        expect(settings.video_device_name).toBe("secondary")
        // A legacy "H264" preference is the OpenH264 encoder.
        expect(settings.video_encoder).toBe("OpenH264")
        // Re-renders of the same epoch never submit again.
        rerender({ preferences: saved, autoSubmit: true })
        expect(onSubmit).toHaveBeenCalledTimes(1)
    })

    it("waits for the saved preferences instead of submitting defaults", () => {
        const { onSubmit, rerender } = renderDialog({ preferences: null, autoSubmit: false })
        expect(onSubmit).not.toHaveBeenCalled()
        rerender({ preferences: saved, autoSubmit: true })
        expect(onSubmit).toHaveBeenCalledTimes(1)
        expect(onSubmit.mock.calls[0][0].video_device_name).toBe("secondary")
    })

    it("stays open for the user when the saved display is gone after the restart", () => {
        const { onSubmit, onAutoSubmitDeclined } = renderDialog({
            initData: makeInitData("e2", ["primary"]),
            preferences: saved,
            autoSubmit: true,
        })
        expect(onSubmit).not.toHaveBeenCalled()
        expect(onAutoSubmitDeclined).toHaveBeenCalledTimes(1)
    })

    it("never auto-submits without autoSubmit", () => {
        const { onSubmit } = renderDialog({ preferences: saved, autoSubmit: false })
        expect(onSubmit).not.toHaveBeenCalled()
    })
})

describe("resumeKeepsSavedChoices", () => {
    const values = {
        ...DESK_CONFIG_DEFAULTS,
        image_capture: "DXGI",
        video_device_name: "secondary",
        video_encoder: "H264",
        enable_audio: false,
    }

    it("accepts a form that still carries every saved choice", () => {
        expect(resumeKeepsSavedChoices(values, saved)).toBe(true)
    })

    it("rejects any choice replaced by a fallback", () => {
        expect(resumeKeepsSavedChoices({ ...values, video_encoder: "X264" }, saved)).toBe(false)
        expect(resumeKeepsSavedChoices({ ...values, image_capture: "WGC" }, saved)).toBe(false)
        expect(resumeKeepsSavedChoices({ ...values, video_device_name: "primary" }, saved)).toBe(false)
        const withAudio: DeskDevicePreferencesV1 = {
            ...saved,
            captureAudio: true,
            audioCapture: "WASAPI",
            audioDevice: { audioDataFlow: "Render", audioDeviceId: "headset" },
        }
        expect(resumeKeepsSavedChoices({ ...values, enable_audio: false }, withAudio)).toBe(false)
        expect(resumeKeepsSavedChoices({
            ...values,
            enable_audio: true,
            audio_capture: "WASAPI",
            audio_device: { audio_data_flow: "Render", audio_device_id: null },
        }, withAudio)).toBe(false)
    })

    it("does not resume without saved preferences", () => {
        expect(resumeKeepsSavedChoices(values, null)).toBe(false)
    })
})

describe("session resume slot", () => {
    it("expires after its TTL", () => {
        const slot = createSessionResumeSlot(1_000)
        slot.arm({ hadControl: true, clipboard: false, armedAt: 0 })
        expect(slot.peek(1_000)).not.toBeNull()
        expect(slot.peek(1_001)).toBeNull()
        // Expiry is permanent.
        expect(slot.peek(0)).toBeNull()
    })

    it("is cleared explicitly", () => {
        const slot = createSessionResumeSlot()
        slot.arm({ hadControl: false, clipboard: null, armedAt: Date.now() })
        slot.clear()
        expect(slot.peek()).toBeNull()
    })
})
