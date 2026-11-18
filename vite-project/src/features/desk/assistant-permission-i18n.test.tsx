import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { createInstance } from 'i18next';
import { I18nextProvider } from 'react-i18next';
import { describe, expect, it, vi } from 'vitest';
import zh from '@/locales/zh-CN/pages';
import en from '@/locales/en-US/pages';
import type { PermissionRequestDto } from '@/services/types';
import { AssistantPermissionRequest } from './assistant-permission-request';
import { permissionEffectLabel, permissionOperationLabel, permissionReasonLabel, permissionResourceLabel, permissionToolLabel, isReusableWebHost } from './assistant-permission-labels';

describe('permission internationalization', () => {
    it('switches language without changing selected authorization identifiers', async () => {
        const i18n = createInstance();
        await i18n.init({ lng: 'zh-CN', fallbackLng: 'en-US',
            resources: { 'zh-CN': { translation: zh }, 'en-US': { translation: en } } });
        const request: PermissionRequestDto = {
            schemaVersion: 1, createdAt: '2026-09-19T00:00:00Z',
            requestId: 'translated', inputRevision: 1, state: 'pending', items: [{
                itemId: 'read', providerId: 'desktop.session', toolName: 'inspect_desktop_session',
                reason: 'User-provided reason', expectedEffect: 'read_device',
                resourceScope: ['target:current_device', 'target:another-device'], operationScope: ['observe'],
                exportDestinations: [], suggestedMaxUses: 2, suggestedTtlSeconds: 300,
            }],
        };
        const onDecide = vi.fn().mockResolvedValue(true);
        render(<I18nextProvider i18n={i18n}><AssistantPermissionRequest request={request} canDecide onDecide={onDecide} /></I18nextProvider>);
        expect(screen.getAllByText('查看桌面会话').length).toBeGreaterThan(0);
        expect(screen.getByRole('checkbox', { name: '当前被控设备' })).toBeChecked();
        expect(screen.getByRole('checkbox', { name: '查看信息' })).toBeChecked();
        fireEvent.click(screen.getByRole('checkbox', { name: '设备：another-device' }));
        await act(() => i18n.changeLanguage('en-US'));
        expect(screen.getByRole('checkbox', { name: 'Current target device' })).toBeChecked();
        expect(screen.getByRole('checkbox', { name: 'Device: another-device' })).not.toBeChecked();
        expect(screen.getByRole('checkbox', { name: 'Observe' })).toBeChecked();
        const approveText = i18n.t('pages.aiAssistant.permissionSubmitSelection');
        fireEvent.click(screen.getByRole('button', { name: approveText }));
        await waitFor(() => expect(onDecide).toHaveBeenCalledWith(request, [{
            itemId: 'read', decision: 'approve', resource_scope: ['target:current_device'],
            operation_scope: ['observe'], export_destinations: [], ttl_seconds: 300, max_uses: 2,
        }]));
    });

    it('names file changes by what the tool does, not the shared output category', async () => {
        const i18n = createInstance();
        await i18n.init({ lng: 'zh-CN', fallbackLng: 'en-US',
            resources: { 'zh-CN': { translation: zh }, 'en-US': { translation: en } } });
        const request: PermissionRequestDto = {
            schemaVersion: 1, createdAt: '2026-11-18T00:00:00Z',
            requestId: 'restore', inputRevision: 1, state: 'pending', items: [{
                itemId: 'restore', providerId: 'file.text', toolName: 'restore_command_changes',
                reason: "Requested by the assistant's tool call.", expectedEffect: 'write_artifact',
                resourceScope: ['selected:sha256:abc'], operationScope: ['create_new_artifact'],
                exportDestinations: [], suggestedMaxUses: 1, suggestedTtlSeconds: 300,
            }],
        };
        const onDecide = vi.fn().mockResolvedValue(true);
        render(<I18nextProvider i18n={i18n}><AssistantPermissionRequest request={request} canDecide onDecide={onDecide} /></I18nextProvider>);
        expect(screen.getByText('助手调用工具时发起的申请。')).toBeInTheDocument();
        expect(screen.queryByText("Requested by the assistant's tool call.")).toBeNull();
        expect(screen.getByText(/恢复命令修改 · 恢复文件/)).toBeInTheDocument();
        expect(screen.queryByText(/创建输出文件/)).toBeNull();
        expect(screen.getByText('所选对象：sha256:abc · 恢复命令改动的文件')).toBeInTheDocument();
        expect(screen.queryByText(/创建新文件/)).toBeNull();

        for (const [tool, effect, operation] of [
            ['update_text_file', '修改文件', '修改所选文件'],
            ['delete_text_file', '删除文件', '删除所选文件'],
            ['restore_text_file', '恢复文件', '恢复所选文件'],
        ]) {
            expect(permissionEffectLabel(i18n.t, 'write_artifact', tool)).toBe(effect);
            expect(permissionOperationLabel(i18n.t, 'create_new_artifact', tool)).toBe(operation);
        }
        // Tools that do create output keep the shared wording.
        expect(permissionEffectLabel(i18n.t, 'write_artifact', 'create_text_file')).toBe('创建输出文件');
        expect(permissionOperationLabel(i18n.t, 'create_new_artifact', 'create_text_file')).toBe('创建新文件');
        expect(permissionEffectLabel(i18n.t, 'write_artifact')).toBe('创建输出文件');
        expect(permissionReasonLabel(i18n.t, '检查日志')).toBe('检查日志');
    });

    it('preserves unknown identifiers and distinct resource identities', async () => {
        const i18n = createInstance();
        await i18n.init({ lng: 'zh-CN', fallbackLng: 'en-US',
            resources: { 'zh-CN': { translation: zh } } });
        expect(permissionToolLabel(i18n.t, 'future_tool')).toBe('future_tool');
        expect(permissionOperationLabel(i18n.t, 'ui:future_action')).toBe('ui:future_action');
        expect(permissionResourceLabel(i18n.t, 'unknown:path:keep')).toBe('unknown:path:keep');
        expect(permissionResourceLabel(i18n.t, 'selected:sha256:abc')).toBe('所选对象：sha256:abc');
        expect(permissionResourceLabel(i18n.t, 'selected:sha256:def')).toBe('所选对象：sha256:def');
        expect(permissionOperationLabel(i18n.t, 'launch_application')).toBe('启动应用');
        expect(permissionOperationLabel(i18n.t, 'fetch_public_web')).toBe('读取公开网页');
        expect(permissionResourceLabel(i18n.t, 'external_web_host:example.com')).toBe('网页域名：example.com');
        expect(isReusableWebHost({ toolName: 'fetch_public_web_page', resourceScope: ['external_web_host:example.com'] })).toBe(true);
        expect(isReusableWebHost({ toolName: 'fetch_public_web_page', resourceScope: ['external_url_input:sha256:old'] })).toBe(false);
    });
});
