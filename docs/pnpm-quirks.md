# Notes from reading pnpm lockfiles

## pnpm ships a database of fixes for published packages

The npm manifest for `babel-plugin-transform-typescript-metadata@0.3.2` declares one dependency, `@babel/helper-plugin-utils`, and no peers. Add that release to a project with pnpm 10.12.4, and its entry under `packages` in the lockfile also contains these fields. [Published manifest](https://registry.npmjs.org/babel-plugin-transform-typescript-metadata/0.3.2)

```yaml
peerDependencies:
  '@babel/core': ^7
  '@babel/traverse': ^7
peerDependenciesMeta:
  '@babel/traverse':
    optional: true
```

pnpm 10.12.4 includes Yarn's package-extension database. A rule matches releases of this plugin up to and including 0.3.2 and supplies both declarations during resolution. We repeated the same manifest edit from the same previous lockfile with `ignoreCompatibilityDb: true`. Both fields disappeared from the package record. These are two lockfile representations of the same published tarball. [Matching rule in Yarn](https://github.com/yarnpkg/berry/blob/%40yarnpkg/extensions/2.0.3/packages/yarnpkg-extensions/sources/index.ts#L963-L971) · [pnpm's compatibility database setting](https://pnpm.io/10.x/settings#ignorecompatibilitydb)

The lockfile does not record the setting. Turning it on with no other change left the lockfile as it was, peers included. The declarations disappeared only once a manifest edit made pnpm resolve again.

## An automatic peer can come from another project's lock entry

One project in a workspace depends on `react@16.14.0`, and the lockfile records it. A second project adds `react-dom@18.2.0`, whose peer range is `^18.2.0`, and declares no React of its own. `autoInstallPeers` is on, so pnpm installs the missing peer.

| pnpm | `react-dom` in the second project |
| --- | --- |
| 9.15.5, 10.12.4, 10.34.6 | `18.2.0(react@16.14.0)` |
| 11.28.11 | `18.2.0(react@18.3.1)` |

pnpm 9 and 10 fill a missing peer with the newest release of that name already in the lockfile, whatever range asked for it. React 16 is the only one there, so it becomes the provider, and the install reports an unmet peer. The lockfile passes `--frozen-lockfile` all the same.

Optional peers follow their own rule. In a workspace that locks `test-host@1.1.0`, two new projects each add a package. One requires `test-host@^2` as a peer, the other declares `test-host@^1` as an optional peer. pnpm 9.15.5, 10.12.4 and 10.34.6 used 1.1.0 for both. pnpm 12.21.0 resolved a new 2.0.0 for the required peer and reused 1.1.0 for the optional one. The optional peer was omitted when the lock had no compatible release. [pnpm 10.12.4 peer selection](https://github.com/pnpm/pnpm/blob/v10.12.4/pkg-manager/resolve-dependencies/src/hoistPeers.ts)

## A catalog's `version` can contain a range

An app declares `"local-sdk": "catalog:"`. The catalog says `^1.0.0-beta.2`, and the workspace contains a matching `local-sdk` at `1.0.0-beta.2`. With `linkWorkspacePackages: true`, every version below gives the app the same local directory.

| pnpm | `catalogs.default.local-sdk.version` | App's resolved dependency |
| --- | --- | --- |
| 9.15.9, 10.34.9, 11.28.12 | `1.0.0-beta.2` | `link:../sdk` |
| 12.0.0, 12.5.1 | Catalog entry omitted | `link:../sdk` |
| 12.6.0, 12.21.0 | `^1.0.0-beta.2` | `link:../sdk` |

On 12.21.0, the catalog's `specifier` and `version` both contain the caret. The app's lockfile entry points to the directory, and the package manifest in that directory supplies the exact version. [pnpm catalogs](https://pnpm.io/catalogs)

## A development dependency changes groups across pnpm versions

A workspace app lists `sdk: ^1` in both `devDependencies` and `peerDependencies`. A root override points `sdk` at the local SDK directory. The SDK resolves to the same path on both runs. Upgrading pnpm changes the app's entry in the lockfile.

```diff
 # pnpm 12.16.1 → 12.17.0
 importers:
   app:
-    dependencies:
+    devDependencies:
       sdk:
         specifier: link:../sdk
         version: link:../sdk
```

The dependency group is part of what a frozen install checks. A lockfile can point to the right package at the right path and still be rejected for putting it in the wrong group. [pnpm 12.17 release notes](https://github.com/pnpm/pnpm/releases/tag/v12.17.0)

## A hook can change the dependency graph while its checksum stays the same

A `.pnpmfile.cjs` hook reads a file named `choice` and adds the package named inside it to a plugin's dependencies. In a pnpm 10.12.4 workspace, `choice` initially contained `child`. We changed it to `host` and edited the root manifest so pnpm ran resolution again.

The plugin's snapshot changed from `child: 1.0.0` to `host: 1.0.0`. The recorded `pnpmfileChecksum` stayed identical. Editing the hook file itself then changed the checksum. pnpm 9.14.4 showed the same behavior. The checksum identifies the hook file. Other inputs the JavaScript reads can affect its result. [pnpmfile hooks](https://pnpm.io/10.x/pnpmfile)

The edit does not have to touch the plugin's project. With the plugin in one workspace project, adding a dependency to another project also moved the plugin's snapshot to the new answer. Once pnpm resolves, it runs `readPackage` for every package again. With no manifest edit at all, pnpm 10.12.4 left the lockfile unchanged.

## A package's own dependency can hide a peer declaration

A plugin lists `child: "1"` in both `dependencies` and `peerDependencies`. The app using the plugin supplies no `child` of its own. The plugin resolves `child@1.0.0` from its ordinary dependencies.

With pnpm 11.28.6, the plugin's snapshot contains `dependencies.child: 1.0.0`, while the package record omits `peerDependencies.child`. With 11.28.7, the same snapshot edge remains and the package record also contains `peerDependencies.child: "1"`. The same change appears between 12.14.0 and 12.15.0. Keeping the declaration matters on a later install when the app starts supplying the peer itself. [Upstream reproduction](https://github.com/pnpm/pnpm/issues/17056) · [pnpm 11.28.7](https://github.com/pnpm/pnpm/releases/tag/v11.28.7) · [pnpm 12.15](https://github.com/pnpm/pnpm/releases/tag/v12.15.0)
