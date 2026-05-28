Yes — **but the practical fork target is YACReaderLibraryServer, not the iOS app**.

Your screenshot is YACReader iOS asking for a **YACReaderLibrary/YACReaderLibraryServer** IP + port. With Tailscale, you can point that to your server’s Tailscale IP instead of a LAN IP like `192.168.2.100`.

```text
iPhone YACReader app
  ↓ Tailscale private network
YACReaderLibraryServer
  ↓ same comic files
/srv/comics
  ↑
Stump
```

## The important catch

The public `YACReader/yacreader` repo is **not the iOS app source**. The repo says it contains the **desktop version**, and the tree includes `YACReaderLibraryServer`. It is GPL-3.0, C++/Qt/CMake, and the README says PRs should target the `develop` branch.

So:

| Goal                                                                                    |                                                  Feasible? | How |
| --------------------------------------------------------------------------------------- | ---------------------------------------------------------: | --- |
| Make YACReader iOS connect directly to Stump                                            | **Probably no**, unless iOS source is available separately |     |
| Make YACReader iOS keep using YACReaderLibraryServer, while the server syncs with Stump |                                                    **Yes** |     |
| Sync reading progress between YACReader and Stump                                       |                                   **Yes, likely feasible** |     |
| Sync Stump metadata/collections into YACReader perfectly                                |                                                 **Harder** |     |
| Add Stump OPDS browsing into YACReader iOS                                              |                             **Probably not via this repo** |     |

## Best architecture

I would **not fork the iOS reader**. I would fork or extend **YACReaderLibraryServer** so the normal YACReader app still thinks it is talking to a regular YACReader server.

```text
YACReader iOS
  ⇅ existing YACReader sync protocol
YACReaderLibraryServer fork
  ⇅ new Stump sync module
Stump GraphQL / OPDS APIs
```

That way you keep using the App Store YACReader app, including its panel-by-panel reader, but your server does extra sync work behind the scenes.

## Why this is realistic

YACReader iOS already syncs with YACReaderLibrary. The iOS guide says YACReaderLibrary can be used to browse, import, and read comics remotely, and comics imported this way can sync back to YACReaderLibrary. ([YACReader][1]) It also says the iOS app connects by entering the server IP and port, which is exactly the screen you showed. ([YACReader][1])

Stump already has useful progress primitives. In Stump mobile’s code, progress sync pushes a GraphQL mutation called `updateMediaProgress(id, input)`, and for paged comics it sends a `paged` progress object with `page` and `elapsedSeconds`.  

Stump also has a pull-side query that fetches `readProgress`, including `page`, `percentageCompleted`, `updatedAt`, `elapsedSeconds`, and Readium locator fields.  Its local mobile schema stores downloaded file IDs, page number, percentage, elapsed seconds, last modified timestamp, and sync status, which are exactly the kinds of fields you need for two-way sync. 

So the Stump side is not the blocker. The bigger work is mapping YACReader’s library/comic IDs to Stump media IDs safely.

## Recommended implementation plan

### Phase 1 — no fork yet

Run both servers against the same comics folder:

```text
/srv/comics
  ├── Batman/
  ├── Manga/
  └── Graphic Novels/
```

Then:

```text
Stump scans /srv/comics
YACReaderLibraryServer scans /srv/comics
```

Use Tailscale IPs:

```text
YACReader iOS → 100.x.y.z:8080
Stump / Panels → 100.x.y.z:10801 or your Stump port
```

This gives you private-network access immediately, without opening ports publicly.

### Phase 2 — build a sidecar sync service first

Before forking YACReader, I’d build a small sidecar service:

```text
yac-stump-sync
  reads YACReader library database / API
  reads/writes Stump GraphQL progress
  matches files by relative path + size/hash
  syncs page progress
```

Start with **one-way sync**:

```text
YACReader progress → Stump progress
```

That is probably your most useful flow if YACReader is your preferred reader.

Then add the reverse:

```text
Stump / Panels progress → YACReader progress
```

Two-way sync needs conflict rules:

```text
if YACReader.updated_at > Stump.updated_at:
    push YACReader progress to Stump
else:
    pull Stump progress into YACReader
```

### Phase 3 — fork YACReaderLibraryServer only if needed

Once the sidecar proves the mapping is correct, move the logic into a YACReaderLibraryServer fork.

That would give you a cleaner setup:

```text
yacreaderlibraryserver-stump
  --stump-url http://100.x.y.z:10801
  --stump-token ...
  --sync-progress true
  --sync-direction two-way
```

The YACReader project explicitly asks people planning larger contributions to contact the maintainer first, and says PRs should target `develop`, so if the goal is upstreaming, it would be worth opening an issue/discussion before coding deeply.

## What I would sync first

Start very small:

| Data                  | Sync priority | Notes                                                       |
| --------------------- | ------------: | ----------------------------------------------------------- |
| Current page          |      **High** | Most useful, easiest                                        |
| Read/unread/completed |      **High** | Can derive from page/percent                                |
| Last read timestamp   |      **High** | Needed for conflict handling                                |
| Total pages           |        Medium | Useful for percentage matching                              |
| Bookmarks             |   Medium/hard | Stump has bookmark/annotation structures, but map carefully |
| Metadata              |           Low | Let each app manage its own metadata at first               |
| Collections/tags      |      Low/hard | Different data models                                       |

## My recommendation

For your setup, I’d do this:

```text
Keep Stump as the canonical server/backend.
Keep YACReaderLibraryServer for YACReader iOS.
Use Tailscale for private access.
Build a small Stump ↔ YACReader progress-sync sidecar first.
Only fork YACReaderLibraryServer after the sidecar proves the mapping works.
```

That is much lower risk than trying to modify the iOS app, and it keeps the normal YACReader app working exactly as-is.

[1]: https://ios.yacreader.com/user-guide/ "User Guide - YACReader for iOS"
