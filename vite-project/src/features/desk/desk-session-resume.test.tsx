import { act, render } from '@testing-library/react';
import '@testing-library/jest-dom';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React, { useEffect } from 'react';
import DeskSession from './desk-session';
import { createSessionResumeSlot } from './session-resume';
import { DEFAULT_DESK_DEVICE_PREFERENCES } from './desk-preferences';

// Session resume across the remount a host restart causes. Mirrors the
// timeline reproduced on device: the signaling socket of a live session drops,
// the runtime remounts for the re-registered device, that mount is torn down
// again before it connects, and a later mount finally resumes.

const signalingHarness = vi.hoisted(() => ({
  subscribers: new Set<(message: any) => void>(),
  sendMessage: vi.fn((_type: number, _data: unknown, _target?: string) => 'admission-request'),
  sendTracked: vi.fn(() => ({ requestId: 'r', disposition: 'sent' })),
  cancelQueued: vi.fn(),
  cancelQueuedScope: vi.fn(),
}));
const fullscreenHarness = vi.hoisted(() => ({
  connected: true,
  toast: vi.fn(),
  t: vi.fn((key: string, fallback?: unknown) => typeof fallback === 'string' ? fallback : key),
}));
const resumeHarness = vi.hoisted(() => ({
  signalingConnected: true,
  lastCloseCode: null as number | null,
  rtcConnected: false,
  initData: null as any,
  connect: vi.fn(),
  slot: null as any,
  dialogSubmits: [] as string[],
  dialogMode: 'submit' as 'submit' | 'decline',
}));
vi.mock('@/hooks/use-toast', () => ({ useToast: () => ({ toast: fullscreenHarness.toast }) }));
const targetHarnessBox = vi.hoisted(() => ({}));
void targetHarnessBox;
const targetHarness = vi.hoisted(() => ({
  assertCurrent: vi.fn(async () => {}),
  refresh: vi.fn(async () => {}),
  remount: vi.fn(),
  discoveryRevision: 0,
}));
// Mock routing
vi.mock('react-router-dom', () => ({
  useParams: () => ({ id: 'test-desk' }),
  useNavigate: () => vi.fn(),
}));

// Mock translations
vi.mock('react-i18next', () => ({
  useTranslation: () => ({
    t: fullscreenHarness.t,
  }),
}));

// Keep the control-bar tests independent of the assistant connection query.
vi.mock('@/services/hooks/connectionController/useListConnections', () => ({
  useListConnections: () => ({ data: { success: true, code: 0, data: [] } }),
}));

vi.mock('@/services/hooks/systemController/useQueryServerInfo', () => ({
  useQueryServerInfo: () => ({ data: { data: { ai_assistant: null } } }),
}));

// Mock hooks
vi.mock('./use-desk-signaling', () => ({
  useDeskSignaling: () => ({
    isConnected: resumeHarness.signalingConnected,
    lastCloseCode: resumeHarness.lastCloseCode,
    subscribe: (handler: (message: any) => void) => {
      signalingHarness.subscribers.add(handler);
      return () => signalingHarness.subscribers.delete(handler);
    },
    sendMessage: signalingHarness.sendMessage,
    sendTracked: signalingHarness.sendTracked,
    cancelQueued: signalingHarness.cancelQueued,
    cancelQueuedScope: signalingHarness.cancelQueuedScope,
  }),
}));

const rtcInputRefs = vi.hoisted(() => ({
  peerConnection: { current: null }, mouseChannel: { current: null },
  keyboardChannel: { current: null }, mouseMoveChannel: { current: null },
}));

vi.mock('./use-desk-rtc', () => ({
  useDeskRTC: () => ({
    ...rtcInputRefs,
    isRTCConnected: resumeHarness.rtcConnected,
    initData: resumeHarness.initData,
    rtcStats: { fps: 60, bitrate: 1000 },
    connect: resumeHarness.connect,
    closeRTC: vi.fn(),
  }),
}));

vi.mock('./use-desk-input', () => ({
  useDeskInput: () => ({
    releaseForLayout: vi.fn(),
    sendMouseIntent: vi.fn(),
    sendKeyboardEvents: vi.fn(),
    releaseAllInputs: vi.fn(),
  }),
}));

vi.mock('./use-desk-clipboard', () => ({
  useDeskClipboard: () => ({
    clipboardEnabled: false,
    transferStatus: 'idle',
    fallbackToast: { show: false },
    toggleClipboard: vi.fn(),
    enableClipboard: vi.fn(),
  }),
}));

vi.mock('./use-desk-whiteboard', () => ({
  useDeskWhiteboard: () => ({
    cancelStroke: vi.fn(), reflowTextInput: vi.fn(),
    isActive: false,
    isInteractive: false,
    canActivate: true,
    elements: [],
    toggleWhiteboard: vi.fn(),
    deactivateWhiteboard: vi.fn(),
  }),
}));

vi.mock('./use-desk-microphone', () => ({
  useDeskMicrophone: () => ({
    isMicActive: false,
    toggleMicrophone: vi.fn(),
  }),
}));

vi.mock('./use-cursor-sync', () => ({
  useCursorSync: () => ({
    cursorStyle: 'default',
  }),
}));


vi.mock('./device-target-context', async (importOriginal) => ({
  ...(await importOriginal<typeof import('./device-target-context')>()),
  useDeviceTarget: () => ({
    kind: 'device', connectionId: 'test-desk', deviceId: 'standalone-test-host',
    basePath: '/desk/device/standalone-test-host',
    assertCurrent: targetHarness.assertCurrent, refresh: targetHarness.refresh,
    remount: targetHarness.remount,
    discoveryRevision: targetHarness.discoveryRevision,
    resume: resumeHarness.slot,
    resolution: { status: 'persistent', deviceKey: 'device:standalone-test-host' },
  }),
}));

// Mock ResizeObserver
globalThis.ResizeObserver = class ResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
} as any;

declare const require: any;

// Mock Popover to reliably test onOpenChange
vi.mock('@/components/ui/popover', () => {
  const React = require('react');
  return {
    Popover: ({ children, onOpenChange }: any) => (
      <div 
        data-testid="mock-popover" 
        onClick={() => onOpenChange?.(true)}
      >
        {children}
      </div>
    ),
    PopoverTrigger: ({ children, asChild }: any) => {
      if (asChild) {
        return React.cloneElement(children, { 'data-popover-trigger': 'true' });
      }
      return <button data-popover-trigger="true">{children}</button>;
    },
    PopoverContent: ({ children }: any) => <div>{children}</div>,
  };
});

// Mock DropdownMenu to prevent real Radix from firing onOpenChange(false) on blur
vi.mock('@/components/ui/dropdown-menu', () => {
  return {
    DropdownMenu: ({ children }: any) => <div>{children}</div>,
    DropdownMenuTrigger: ({ children, asChild }: any) => {
      const React = require('react');
      if (asChild) return React.cloneElement(children, { 'data-dropdown-trigger': 'true' });
      return <button>{children}</button>;
    },
    DropdownMenuSeparator: () => <hr />,
  DropdownMenuContent: ({ children }: any) => <div>{children}</div>,
    DropdownMenuItem: ({ children }: any) => <div>{children}</div>,
  };
});

// The real dialog's auto-submit is covered in desk-config-auto-resume.test.tsx;
// here a stub submits as soon as the session allows it, so the test exercises
// the session's arm / claim / clear wiring.
vi.mock('./desk-config-dialog', () => ({
  DeskConfigDialog: (props: any) => {
    useEffect(() => {
      if (!props.open || !props.autoSubmit) return;
      if (resumeHarness.dialogMode === 'decline') {
        props.onAutoSubmitDeclined?.();
        return;
      }
      resumeHarness.dialogSubmits.push('resume');
      props.onSubmit({
        image_capture: 'DXGI', video_device_name: 'primary', video_encoder: 'X264',
        video_quality: 22, video_fps: 60, show_mouse: true, enable_dirty_rect: true,
        adaptive_bitrate: true, adaptive_web_page_resolution: false,
        wayland_control_mode: 'auto', audio: null,
      }, DEFAULT_DESK_DEVICE_PREFERENCES, 'resume');
    }, [props.open, props.autoSubmit]);
    return props.open ? (
      <div data-testid="config-dialog">
        <button onClick={() => props.onSubmit({
          image_capture: 'DXGI', video_device_name: 'primary', video_encoder: 'X264',
          video_quality: 22, video_fps: 60, show_mouse: true, enable_dirty_rect: true,
          adaptive_bitrate: true, adaptive_web_page_resolution: false,
          wayland_control_mode: 'auto', audio: null,
        }, DEFAULT_DESK_DEVICE_PREFERENCES, 'user')}>user-submit</button>
        <button onClick={() => props.onCancel()}>user-cancel</button>
      </div>
    ) : null;
  },
}));

const initData = (epoch: string) => ({
  connection_epoch: epoch,
  video_device_list: {},
  audio_device_list: {},
  video_encoder_list: ['X264'],
  audio_encoder_list: [],
  suggested_session_settings: { adaptive_bitrate: true },
  session_settings_capabilities: { adaptive_bitrate: 'apply' },
});

function liveSession() {
  resumeHarness.signalingConnected = true;
  resumeHarness.rtcConnected = true;
  resumeHarness.initData = initData('e1');
  return render(<DeskSession />);
}

describe('DeskSession resume after a host restart', () => {
  beforeEach(() => {
    resumeHarness.slot = createSessionResumeSlot();
    resumeHarness.lastCloseCode = null;
    targetHarness.remount.mockReset();
    resumeHarness.connect.mockReset();
    resumeHarness.dialogSubmits = [];
    resumeHarness.dialogMode = 'submit';
    (globalThis as any).RTCPeerConnection = class {};
  });
  afterEach(() => {
    delete (globalThis as any).RTCPeerConnection;
  });

  it('arms on a signaling drop of a live session and resumes in the next runtime', async () => {
    const a = liveSession();
    resumeHarness.signalingConnected = false;
    a.rerender(<DeskSession />);
    expect(resumeHarness.slot.peek()).not.toBeNull();
    a.unmount();

    // Mount 2: the re-registered device; torn down before it connects.
    resumeHarness.signalingConnected = true;
    resumeHarness.rtcConnected = false;
    resumeHarness.initData = initData('e2');
    const b = render(<DeskSession />);
    await act(async () => {});
    expect(resumeHarness.connect).toHaveBeenCalledTimes(1);
    expect(resumeHarness.slot.peek()).not.toBeNull();
    b.unmount();

    // Mount 3 still resumes, and the link coming up fulfils the intent.
    resumeHarness.initData = initData('e3');
    const c = render(<DeskSession />);
    await act(async () => {});
    expect(resumeHarness.connect).toHaveBeenCalledTimes(2);
    resumeHarness.rtcConnected = true;
    c.rerender(<DeskSession />);
    expect(resumeHarness.slot.peek()).toBeNull();
    c.unmount();
  });

  /** Regression (fast user switch, on device 2026-11-20): the host evicted
   *  the page with "session changed"; the socket would reconnect on this same
   *  mount, which never claims the resume intent, so the page stayed stuck. */
  it('remounts with the intent armed when the host moved the session off the console', () => {
    const view = liveSession();
    resumeHarness.signalingConnected = false;
    resumeHarness.lastCloseCode = 4002;
    view.rerender(<DeskSession />);
    expect(resumeHarness.slot.peek()).not.toBeNull();
    expect(targetHarness.remount).toHaveBeenCalledTimes(1);
    view.unmount();
  });

  it('does not remount on an ordinary signaling drop', () => {
    const view = liveSession();
    resumeHarness.signalingConnected = false;
    resumeHarness.lastCloseCode = 1006;
    view.rerender(<DeskSession />);
    expect(targetHarness.remount).not.toHaveBeenCalled();
    view.unmount();
  });

  it('does not arm before the session ever had a live media link', () => {
    resumeHarness.signalingConnected = true;
    resumeHarness.rtcConnected = false;
    resumeHarness.initData = initData('e1');
    const view = render(<DeskSession />);
    resumeHarness.signalingConnected = false;
    view.rerender(<DeskSession />);
    expect(resumeHarness.slot.peek()).toBeNull();
    view.unmount();
  });

  it('withdraws the intent only when the same runtime is handed back its epoch', () => {
    const view = liveSession();
    resumeHarness.signalingConnected = false;
    view.rerender(<DeskSession />);
    expect(resumeHarness.slot.peek()).not.toBeNull();
    // A signaling reconnect alone proves nothing about the host session.
    resumeHarness.signalingConnected = true;
    view.rerender(<DeskSession />);
    expect(resumeHarness.slot.peek()).not.toBeNull();
    // The host re-initialises the same epoch: the session survived.
    resumeHarness.initData = initData('e1');
    view.rerender(<DeskSession />);
    expect(resumeHarness.slot.peek()).toBeNull();
    view.unmount();
  });

  it('keeps the intent when a runtime declines, so a later runtime can resume', async () => {
    const a = liveSession();
    resumeHarness.signalingConnected = false;
    a.rerender(<DeskSession />);
    a.unmount();

    resumeHarness.signalingConnected = true;
    resumeHarness.rtcConnected = false;
    resumeHarness.dialogMode = 'decline';
    resumeHarness.initData = initData('e2');
    const b = render(<DeskSession />);
    await act(async () => {});
    expect(resumeHarness.connect).not.toHaveBeenCalled();
    expect(b.getByTestId('config-dialog')).toBeInTheDocument();
    expect(resumeHarness.slot.peek()).not.toBeNull();
    b.unmount();

    resumeHarness.dialogMode = 'submit';
    resumeHarness.initData = initData('e3');
    const c = render(<DeskSession />);
    await act(async () => {});
    expect(resumeHarness.connect).toHaveBeenCalledTimes(1);
    c.unmount();
  });

  it.each(['user-submit', 'user-cancel'])('clears the intent on %s', async (action) => {
    const a = liveSession();
    resumeHarness.signalingConnected = false;
    a.rerender(<DeskSession />);
    a.unmount();

    resumeHarness.signalingConnected = true;
    resumeHarness.rtcConnected = false;
    resumeHarness.dialogMode = 'decline';
    resumeHarness.initData = initData('e2');
    const b = render(<DeskSession />);
    await act(async () => {});
    expect(resumeHarness.slot.peek()).not.toBeNull();
    await act(async () => { b.getByText(action).click(); });
    expect(resumeHarness.slot.peek()).toBeNull();
    b.unmount();
  });

  it('opens the dialog without submitting when nothing is armed', async () => {
    resumeHarness.signalingConnected = true;
    resumeHarness.rtcConnected = false;
    resumeHarness.initData = initData('e1');
    const view = render(<DeskSession />);
    await act(async () => {});
    expect(view.getByTestId('config-dialog')).toBeInTheDocument();
    expect(resumeHarness.dialogSubmits).toEqual([]);
    expect(resumeHarness.connect).not.toHaveBeenCalled();
    view.unmount();
  });
});
