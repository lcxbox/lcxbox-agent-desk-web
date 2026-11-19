import { definePlugin, type PluginFactoryOptions, type UserPluginWithLifeCycle } from '@kubb/core'

type ImportEntry = { path: string; name?: unknown; isTypeOnly?: boolean }

/** Preserve Kubb's import groups without letting a Windows drive change their order. */
export function orderPortableImports<T extends ImportEntry>(imports: T[]): void {
    const key = (entry: T) => entry.path.replaceAll('\\', '/').replace(/^[A-Za-z]:\//, '/')
    imports.sort((left, right) => {
        const group = Number(Array.isArray(left.name)) - Number(Array.isArray(right.name))
            || Number(!left.isTypeOnly) - Number(!right.isTypeOnly)
        if (group) return group
        const a = key(left)
        const b = key(right)
        return a < b ? -1 : a > b ? 1 : 0
    })
}

export const portableImportOrderPlugin = definePlugin(() => ({
    name: 'portable-import-order',
    options: {},
    install() {
        this.fabric.context.on('files:processing:start', files => {
            for (const file of files) orderPortableImports(file.imports ?? [])
        })
    },
}))

/** Kubb's barrel tree splits plugin paths on forward slashes on every OS. */
export function portablePluginPaths<T extends PluginFactoryOptions>(
    plugin: UserPluginWithLifeCycle<T>,
): UserPluginWithLifeCycle<T> {
    const resolvePath = plugin.resolvePath
    if (!resolvePath) return plugin
    return {
        ...plugin,
        resolvePath(...args) {
            return resolvePath.apply(this, args).replaceAll('\\', '/')
        },
    }
}
