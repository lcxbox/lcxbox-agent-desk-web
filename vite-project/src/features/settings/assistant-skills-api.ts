import { useEffect, useState } from 'react';
import {
    createMySkill, createPlatformSkill, deleteMySkill, deletePlatformSkill, getMySkill, getMySkillScan, getMySkillVersion,
    getPlatformSkill, getPlatformSkillScan, getPlatformSkillVersion, listMySkills, listPlatformSkills, previewMySkill, previewMySkillVersion, previewPlatformSkill,
    previewPlatformSkillVersion, updateMySkill, updatePlatformSkill,
} from '@/services/clients';
import type {
    SkillCreate, SkillDetail, SkillPreview, SkillPreviewRequest, SkillSaved, SkillSummary, SkillUpdate, SkillVersionView,
} from '@/services/types';
import { deskErrorCodeEnum } from '@/services/types';
import type { TFunction } from 'i18next';
import { deskErrorMessage, errorCodeOf, SKILL_ERROR_KEYS } from '@/lib/desk-error-i18n';

export type SkillScopeKind = 'platform' | 'personal';

/** The same skill operations against the platform or the personal endpoints. */
export interface SkillApi {
    list(): Promise<SkillSummary[]>;
    detail(skillId: string): Promise<SkillDetail>;
    version(skillId: string, version: number): Promise<SkillVersionView>;
    /** Whether uploads can be AI-scanned now. */
    scanAvailable(): Promise<boolean>;
    preview(request: SkillPreviewRequest, skillId?: string): Promise<SkillPreview>;
    create(request: SkillCreate): Promise<SkillSaved>;
    update(skillId: string, request: SkillUpdate): Promise<SkillSaved>;
    remove(skillId: string, expectedRevision: number): Promise<void>;
}

async function data<T>(response: Promise<{ success: boolean; data?: T | null }>): Promise<T> {
    const body = await response;
    if (!body.success || body.data == null) throw new Error('Skill request failed');
    return body.data;
}

const platform: SkillApi = {
    list: () => data(listPlatformSkills()),
    detail: skillId => data(getPlatformSkill(skillId)),
    version: (skillId, version) => data(getPlatformSkillVersion(skillId, version)),
    scanAvailable: async () => (await data(getPlatformSkillScan())).available,
    preview: (request, skillId) => data(skillId ? previewPlatformSkillVersion(skillId, request) : previewPlatformSkill(request)),
    create: request => data(createPlatformSkill(request)),
    update: (skillId, request) => data(updatePlatformSkill(skillId, request)),
    remove: async (skillId, expectedRevision) => { await data(deletePlatformSkill(skillId, { expectedRevision })); },
};

const personal: SkillApi = {
    list: () => data(listMySkills()),
    detail: skillId => data(getMySkill(skillId)),
    version: (skillId, version) => data(getMySkillVersion(skillId, version)),
    scanAvailable: async () => (await data(getMySkillScan())).available,
    preview: (request, skillId) => data(skillId ? previewMySkillVersion(skillId, request) : previewMySkill(request)),
    create: request => data(createMySkill(request)),
    update: (skillId, request) => data(updateMySkill(skillId, request)),
    remove: async (skillId, expectedRevision) => { await data(deleteMySkill(skillId, { expectedRevision })); },
};

export function skillApi(scope: SkillScopeKind): SkillApi {
    return scope === 'platform' ? platform : personal;
}

export function isSkillConflict(error: unknown): boolean {
    return errorCodeOf(error) === deskErrorCodeEnum.REVISION_CONFLICT;
}

/** The localized failure, or the server's reason for a rejected package. */
export function skillErrorText(t: TFunction, error: unknown): string {
    const code = errorCodeOf(error);
    return deskErrorMessage(t, SKILL_ERROR_KEYS, code, code == null ? null : (error as Error).message, t('pages.assistantSkills.failed'));
}

/** Base64 of a file's bytes, for the JSON zip upload. */
export async function fileBase64(file: Blob): Promise<string> {
    const bytes = new Uint8Array(await file.arrayBuffer());
    let binary = '';
    for (let offset = 0; offset < bytes.length; offset += 0x8000) {
        binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
    }
    return btoa(binary);
}

/** `false` only when the host reports the scan unavailable; an unknown answer keeps the option. */
export function useSkillScanAvailable(api: SkillApi): boolean {
    const [available, setAvailable] = useState(true);
    useEffect(() => {
        let live = true;
        api.scanAvailable().then(value => { if (live) setAvailable(value); }).catch(() => undefined);
        return () => { live = false; };
    }, [api]);
    return available;
}
