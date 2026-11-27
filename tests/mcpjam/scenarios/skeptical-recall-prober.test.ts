// Persona "Skeptical Recall Prober": is what was claimed actually stored, and
// does nothing invented turn up?
import { describe, expect, test, vi } from "vitest";

import { create, remember, target, useScenario } from "../src/harness.js";
import type { ToolError } from "../src/okf.js";
import { SCENARIOS } from "../src/scenarios.js";

describe(SCENARIOS.createAndRetrieve.goal, () => {
    const { session, paths } = useScenario(SCENARIOS.createAndRetrieve);

    test("a stored fact comes back verbatim, by search and by read", async () => {
        const path = paths.at("preferences/coffee-shop");
        const fact = "My favorite coffee shop is called The Daily Brew.";
        await remember(session(), "Remember my favorite coffee shop", [
            { path, title: "Favorite coffee shop", body: `# Favorite coffee shop\n\n${fact}\n` },
        ]);

        expect(await session().searchUnder(paths.folder, "Daily Brew")).toEqual([path]);
        expect((await session().concept(path)).body).toContain(fact);
    });
});

describe(SCENARIOS.scatteredFacts.goal, () => {
    const { session, paths } = useScenario(SCENARIOS.scatteredFacts);
    // One message, one publish each: facts that arrive across a conversation.
    const facts = [
        { name: "dog", term: "Biscuit", text: "My dog is called Biscuit." },
        { name: "birthday", term: "October", text: "My birthday is in October." },
        { name: "hometown", term: "Duluth", text: "I grew up in Duluth." },
    ];

    test("each fact told separately is stored, and found by its own term", async () => {
        for (const { name, text } of facts) {
            await remember(session(), `Remember: ${text}`, [
                { path: paths.at(`me/${name}`), title: name, body: `# ${name}\n\n${text}\n` },
            ]);
        }
        for (const { name, term } of facts) {
            expect(await session().searchUnder(paths.folder, term)).toEqual([paths.at(`me/${name}`)]);
        }
    });

    test("a leading question about a detail never told finds nothing", async () => {
        expect(await session().searchUnder(paths.folder, "parrot")).toEqual([]);
        expect(await session().searchUnder(paths.folder, "Biscuit Duluth")).toEqual([]);
    });
});

describe(SCENARIOS.sourceAttribution.goal, () => {
    const { session, paths } = useScenario(SCENARIOS.sourceAttribution);
    const path = paths.at("history/footbridge");
    // Unique to this run's folder: `cited_by` searches the whole catalog, so a
    // fixed id would also match a concurrent or kept run in a shared catalog.
    const sourceId = `family-diary-1995-10-03${paths.folder.replace(/[^a-z0-9]+/gi, "-")}`;

    test("the source is attached as metadata, not just claimed in prose", async () => {
        const changeset = await session().openChangeset();
        await create(session(), changeset, {
            path,
            title: "Footbridge",
            body: "# Footbridge\n\nThe old footbridge was rebuilt in 1995.\n",
        });
        await session().call("add_source", {
            path,
            id: sourceId,
            resource: "Family diary, entry dated 3 October 1995",
            author: "human:tester",
            changeset,
        });
        await session().publish(changeset, "Remember the footbridge, with its source");

        const concept = await session().concept(path, { raw: true });
        expect(concept.frontmatter.sources).toEqual([
            {
                id: sourceId,
                resource: "Family diary, entry dated 3 October 1995",
                author: "human:tester",
            },
        ]);
        expect(concept.raw).toContain(`id: ${sourceId}`);
        expect(concept.body).not.toContain(sourceId);
    });

    // Issue #761: `cited_by` answers "which notes cite this source?" without
    // reading every concept; `search` never looks at `sources`.
    test("cited_by finds the concepts citing a source id, in any case", async () => {
        for (const id of [sourceId, sourceId.toUpperCase()]) {
            const found = await session().call<{
                hits: { path: string; sources: { id: string }[] }[];
                truncated: boolean;
            }>("cited_by", { source_id: id, catalogs: [session().catalog] });
            // Hit paths come back without the leading slash, as `search` returns them.
            expect(found.hits.map((hit) => `/${hit.path}`)).toEqual([path]);
            expect(found.hits[0]?.sources.map((entry) => entry.id)).toEqual([sourceId]);
            expect(found.truncated).toBe(false);
        }
    });

    // Issue #1114: with no filter, `cited_by` lists the cited sources instead of refusing.
    test("cited_by with no filter lists the cited sources and their counts", async () => {
        const listed = await session().call<{
            hits: unknown[];
            sources: { source_id: string; cited_by: number }[];
            truncated: boolean;
        }>("cited_by", { catalogs: [session().catalog] });
        expect(listed.hits).toEqual([]);
        expect(listed.sources.map((source) => source.source_id)).toContain(sourceId);
        expect(listed.sources.every((source) => source.cited_by >= 1)).toBe(true);
    });

    // Issues #761, #1114, #1118: the `resource` filter and the listing's counts and order.
    // Every id and resource below carries the run's tag, so other sources in a shared
    // catalog never get in the way, and the checks look at this run's sources only.
    const tag = paths.folder.replace(/[^a-z0-9]+/gi, "-");

    interface CitedByHits {
        hits: { path: string; sources: { id?: string; resource?: string }[] }[];
        truncated: boolean;
    }
    async function citedBy(args: Record<string, unknown>): Promise<CitedByHits> {
        return session().call<CitedByHits>("cited_by", { catalogs: [session().catalog], ...args });
    }
    // Hit paths come back without the leading slash.
    const hitPaths = (found: CitedByHits) => found.hits.map((hit) => `/${hit.path}`);

    /** Creates `concepts` (path to its `[id, resource]` citations) and publishes them. */
    async function cite(message: string, concepts: Record<string, [string, string][]>) {
        const changeset = await session().openChangeset();
        for (const [conceptPath, citations] of Object.entries(concepts)) {
            await create(session(), changeset, {
                path: conceptPath,
                title: "Cited note",
                body: "# Cited note\n\nA note that cites sources.\n",
            });
            for (const [id, resource] of citations) {
                await session().call("add_source", { path: conceptPath, id, resource, changeset });
            }
        }
        await session().publish(changeset, message);
    }

    test("cited_by filters by resource (case-sensitive), alone and with source_id", async () => {
        const one = paths.at("cited/resource-one");
        const two = paths.at("cited/resource-two");
        const both = paths.at("cited/resource-both");
        const register = `Parish register${tag}`;
        const ledger = `Estate ledger${tag}`;
        const idOne = `entry-one${tag}`;
        const idTwo = `entry-two${tag}`;
        // `both` cites idOne under one resource and idTwo under the other: each filter alone
        // matches it, but no single entry of it carries idOne with the ledger.
        await cite("Cite sources with resources", {
            [one]: [[idOne, register]],
            [two]: [[idTwo, ledger]],
            [both]: [
                [idOne, ledger],
                [idTwo, register],
            ],
        });
        const sorted = (list: string[]) => [...list].sort();

        await vi.waitFor(async () => {
            expect(sorted(hitPaths(await citedBy({ resource: register })))).toEqual(
                sorted([one, both]),
            );
        });
        expect(hitPaths(await citedBy({ resource: ledger })).sort()).toEqual(sorted([two, both]));
        // The resource is matched exactly: another case and a prefix find nothing.
        expect((await citedBy({ resource: register.toUpperCase() })).hits).toEqual([]);
        expect((await citedBy({ resource: register.slice(0, 6) })).hits).toEqual([]);
        // Together, both must match one entry; the id still ignores case.
        const together = await citedBy({ source_id: idOne.toUpperCase(), resource: register });
        expect(hitPaths(together)).toEqual([one]);
        expect(together.hits[0]?.sources).toEqual([
            expect.objectContaining({ id: idOne, resource: register }),
        ]);
        expect(hitPaths(await citedBy({ source_id: idOne, resource: ledger }))).toEqual([both]);
        expect((await citedBy({ source_id: idTwo, resource: `Other${tag}` })).hits).toEqual([]);
    });

    // Sources for the count and order checks: `popular` is cited by two concepts, and twice
    // by one of them; the others once each, two of them under one id.
    const popular = `z-popular${tag}`;
    const firstId = `a-single${tag}`;
    const sharedId = `b-single${tag}`;
    const counted = new Set([popular, firstId, sharedId].map((id) => id.toLowerCase()));
    const mine = (id: string | null) => counted.has(id?.toLowerCase() ?? "");
    let seeded: Promise<void> | undefined;
    function seedCounts(): Promise<void> {
        // A duplicate id in one concept is refused, so the second citation of `popular` in
        // `first` differs in case only: the same source, as `cited_by` keys it.
        seeded ??= cite("Seed the cited sources", {
            [paths.at("counted/first")]: [
                [popular, `Almanac${tag}`],
                [popular.toUpperCase(), `Almanac${tag}`],
                [firstId, `Atlas${tag}`],
            ],
            [paths.at("counted/second")]: [
                [popular, `Almanac${tag}`],
                [sharedId, `Atlas${tag}`],
            ],
            [paths.at("counted/third")]: [[sharedId, `Gazetteer${tag}`]],
        });
        return seeded;
    }

    interface Listing {
        hits: unknown[];
        sources: { source_id: string | null; resource: string | null; cited_by: number }[];
        truncated: boolean;
    }
    const listSources = (limit?: number) =>
        session().call<Listing>("cited_by", {
            catalogs: [session().catalog],
            ...(limit === undefined ? {} : { limit }),
        });

    // The unfiltered listing is capped at 200 sources for the whole catalog, so this run's
    // sources are only sure to come back when the catalog is the scenario's own (catalog
    // isolation: local mode, or an admin key). In a shared catalog they could fall below the
    // cap whatever the server does, so the next two tests are skipped there.
    const ownCatalog = target().isolation === "catalog";

    test.skipIf(!ownCatalog)("cited_by lists a source once, each citing concept counted once, in count order", async () => {
        await seedCounts();
        let listed: Listing | undefined;
        await vi.waitFor(async () => {
            listed = await listSources();
            expect(listed.sources.filter((source) => mine(source.source_id))).toHaveLength(4);
        });
        expect(listed?.hits).toEqual([]);
        const counts = listed?.sources
            .filter((source) => mine(source.source_id))
            .map((source) => [source.source_id?.toLowerCase(), source.resource, source.cited_by]);
        expect(counts).toEqual([
            // Count first: `popular` sorts last by id but leads. Then id, then resource.
            [popular, `Almanac${tag}`, 2],
            [firstId, `Atlas${tag}`, 1],
            [sharedId, `Atlas${tag}`, 1],
            [sharedId, `Gazetteer${tag}`, 1],
        ]);
    });

    test.skipIf(!ownCatalog)("cited_by cuts the listing at limit and says so", async () => {
        await seedCounts();
        await vi.waitFor(async () => {
            expect((await listSources()).sources.filter((source) => mine(source.source_id))).toHaveLength(4);
        });
        // Four of this run's sources exist, so any limit below four cuts the list.
        const cut = await listSources(2);
        expect(cut.sources).toHaveLength(2);
        expect(cut.truncated).toBe(true);
        expect((await listSources(1)).truncated).toBe(true);
    });

    // A filtered call is not capped by other sources, so this one holds in a shared catalog too.
    test("cited_by cuts the hits of a filtered call at limit and says so", async () => {
        await seedCounts();
        await vi.waitFor(async () => {
            expect(hitPaths(await citedBy({ source_id: popular }))).toHaveLength(2);
        });
        // Two concepts cite `popular`.
        const one = await citedBy({ source_id: popular, limit: 1 });
        expect(hitPaths(one)).toEqual([paths.at("counted/first")]);
        expect(one.truncated).toBe(true);
        expect((await citedBy({ source_id: popular, limit: 2 })).truncated).toBe(false);
    });
});

describe(SCENARIOS.editConflicts.goal, () => {
    const { session, paths } = useScenario(SCENARIOS.editConflicts);
    const path = paths.at("car");

    test("a contradiction edits the one concept, and history shows both versions", async () => {
        await remember(session(), "Remember my car", [
            { path, title: "Car", body: "# Car\n\nMy car is blue.\n" },
        ]);
        const changeset = await session().openChangeset();
        await session().call("edit_document", {
            path,
            old_str: "My car is blue.",
            new_str: "My car is green.",
            changeset,
        });
        await session().publish(changeset, "Correct my car's colour");

        const { body } = await session().concept(path);
        expect(body).toContain("My car is green.");
        expect(body).not.toContain("blue");
        expect(await session().searchUnder(paths.folder, "car")).toEqual([path]);
        const history = await session().call<{ revisions: { op: string; message: string }[] }>(
            "history",
            { path, catalog: session().catalog },
        );
        expect(history.revisions.map(({ op, message }) => [op, message])).toEqual([
            ["update", "Correct my car's colour"],
            ["create", "Remember my car"],
        ]);
    });

    test("an edit based on a stale revision is refused with Conflict at publish", async () => {
        const stale = await session().openChangeset();
        await session().call("edit_document", {
            path,
            old_str: "My car is green.",
            new_str: "My car is red.",
            changeset: stale,
        });
        const winner = await session().openChangeset();
        await session().call("edit_document", {
            path,
            old_str: "My car is green.",
            new_str: "My car is silver.",
            changeset: winner,
        });
        await session().publish(winner, "My car is silver now");

        const conflict = await session().fail("publish", { changeset: stale, message: "red" });
        expect(conflict.error).toBe("Conflict");
        expect((conflict as ToolError & { files: { path: string }[] }).files).toEqual([
            expect.objectContaining({ path }),
        ]);
        expect((await session().concept(path)).body).toContain("My car is silver.");
        await session().call("discard", { changeset: stale });
    });

    // Issues #1126 and #1150: `history` names the revision a `revert` may be guarded with.
    interface History {
        current_revision: number | null;
        latest_revision: number | null;
        revisions: { revision: number; op: string }[];
    }
    const historyOf = (target: string) =>
        session().call<History>("history", { path: target, catalog: session().catalog });
    /** Waits until a replica shows `revision` as the live one (see "Reads after a publish"). */
    const settled = (target: string, revision: number | null) =>
        vi.waitFor(async () => {
            expect((await historyOf(target)).current_revision).toBe(revision);
        });
    /** Creates `target` (revision 1) and edits it (revision 2), each in its own publish. */
    async function twoRevisions(target: string, name: string): Promise<void> {
        await remember(session(), `Create ${name}`, [
            { path: target, title: name, body: `# ${name}\n\n${name} says one.\n` },
        ]);
        await edit(target, `${name} says one.`, `${name} says two.`, `Edit ${name}`);
        await settled(target, 2);
    }
    async function edit(target: string, from: string, to: string, message: string): Promise<void> {
        const changeset = await session().openChangeset();
        await session().call("edit_document", {
            path: target,
            old_str: from,
            new_str: to,
            changeset,
        });
        await session().publish(changeset, message);
    }
    const pendingFiles = async (changeset: string) =>
        (await session().call<{ files: unknown[] }>("pending_changes", { changeset })).files;

    test("history names the live revision twice: current_revision equals latest_revision", async () => {
        const target = paths.at("guard/live");
        await twoRevisions(target, "Live");

        const history = await historyOf(target);
        expect(history.revisions.map(({ revision, op }) => [revision, op])).toEqual([
            [2, "update"],
            [1, "create"],
        ]);
        expect(history.current_revision).toBe(2);
        expect(history.latest_revision).toBe(2);
    });

    test("revert with expected_revision equal to latest_revision succeeds and restores the text", async () => {
        const target = paths.at("guard/restore");
        await twoRevisions(target, "Restore");
        const { latest_revision } = await historyOf(target);

        const changeset = await session().openChangeset();
        await session().call("revert", {
            path: target,
            revision: 1,
            expected_revision: latest_revision,
            changeset,
        });
        // Through the changeset: always current, unlike a read right after a publish.
        expect((await session().concept(target, { changeset })).body).toContain("Restore says one.");
        await session().publish(changeset, "Revert Restore to revision 1");

        await vi.waitFor(async () => {
            expect((await session().concept(target)).body).toContain("Restore says one.");
        });
        expect((await historyOf(target)).revisions[0]).toEqual(
            expect.objectContaining({ revision: 3, op: "update" }),
        );
    });

    test("revert with a stale expected_revision is a Conflict naming the current revision, and writes nothing", async () => {
        const target = paths.at("guard/stale");
        await twoRevisions(target, "Stale");
        const seen = (await historyOf(target)).latest_revision;
        // Someone else publishes after the caller read `history`.
        await edit(target, "Stale says two.", "Stale says three.", "Edit Stale again");
        await settled(target, 3);

        const changeset = await session().openChangeset();
        const refused = await session().fail("revert", {
            path: target,
            revision: 1,
            expected_revision: seen,
            changeset,
        });
        expect(refused.error).toBe("Conflict");
        expect((refused as ToolError & { files: unknown[] }).files).toEqual([
            { path: target, base_revision: 2, current_revision: 3 },
        ]);
        expect(await pendingFiles(changeset)).toEqual([]);
        expect((await session().concept(target)).body).toContain("Stale says three.");
        await session().call("discard", { changeset });
    });

    test("a deleted document has no current_revision; its latest_revision guards a restore", async () => {
        const target = paths.at("guard/deleted");
        await twoRevisions(target, "Deleted");
        const removal = await session().openChangeset();
        await session().call("delete_concept", { path: target, changeset: removal });
        await session().publish(removal, "Delete Deleted");
        await settled(target, null);
        const gone = await historyOf(target);
        expect(gone.current_revision).toBeNull();
        expect(gone.latest_revision).toBe(3);
        expect(gone.revisions[0]).toEqual(expect.objectContaining({ revision: 3, op: "delete" }));

        // The delete's revision is current: the restore is accepted.
        const restore = await session().openChangeset();
        await session().call("revert", {
            path: target,
            revision: 2,
            expected_revision: gone.latest_revision,
            changeset: restore,
        });
        expect(await pendingFiles(restore)).toEqual([
            expect.objectContaining({ path: target, change: "create" }),
        ]);
        await session().call("discard", { changeset: restore });

        // Someone re-creates the document first: the same call is now refused.
        await remember(session(), "Create Deleted again", [
            { path: target, title: "Deleted", body: "# Deleted\n\nDeleted says again.\n" },
        ]);
        await settled(target, 4);
        const late = await session().openChangeset();
        const refused = await session().fail("revert", {
            path: target,
            revision: 2,
            expected_revision: gone.latest_revision,
            changeset: late,
        });
        expect(refused.error).toBe("Conflict");
        expect((refused as ToolError & { files: unknown[] }).files).toEqual([
            { path: target, base_revision: 3, current_revision: 4 },
        ]);
        expect(await pendingFiles(late)).toEqual([]);
        expect((await session().concept(target)).body).toContain("Deleted says again.");
        await session().call("discard", { changeset: late });
    });
});

describe(SCENARIOS.hallucinationAudit.goal, () => {
    const { session, paths } = useScenario(SCENARIOS.hallucinationAudit);
    const anecdotes = [
        { name: "marathon", term: "marathon", text: "I ran a marathon in Lisbon in 2019." },
        { name: "piano", term: "piano", text: "I played piano for ten years as a child." },
        { name: "volcano", term: "volcano", text: "I once hiked up a volcano in Iceland." },
    ];

    test("everything stored is found, and only that", async () => {
        await remember(
            session(),
            "Remember three anecdotes",
            anecdotes.map(({ name, text }) => ({
                path: paths.at(`anecdotes/${name}`),
                title: name,
                body: `# ${name}\n\n${text}\n`,
            })),
        );
        const everything = await session().searchUnder(`${paths.folder}/anecdotes`, "");
        expect(everything.sort()).toEqual(
            anecdotes.map(({ name }) => paths.at(`anecdotes/${name}`)).sort(),
        );
        for (const { name, term } of anecdotes) {
            expect(await session().searchUnder(paths.folder, term)).toEqual([
                paths.at(`anecdotes/${name}`),
            ]);
        }
        for (const invented of ["Antarctica", "skydiving", "Iceland marathon"]) {
            expect(await session().searchUnder(paths.folder, invented)).toEqual([]);
        }
    });
});
