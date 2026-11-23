#!/usr/bin/env python3
"""Assemble the retrieval-v1 ablation corpus and its labelled query set (#981).

Why a generator and not two hand-written files: the experiment's whole claim is
that the corpus splits into a LEXICAL class (the query shares a content term
with its gold section, so BM25 can find it) and a PARAPHRASE class (it shares
none, so only a semantic method can). That split is only worth anything if it
is derived mechanically. So this script derives it with the SAME rule the Rust
loader applies -- `brain_sections::query_terms`, whitespace split, lowercase,
keep terms longer than two characters, no stopword list -- and refuses to emit
a corpus that does not hit the intended counts.

`query_terms` keeps `the`, `and`, `with` (they are three characters and there is
no stopword list), so a paraphrase query has to avoid every such word that
appears in its gold section, not just the rare ones. That is why the paraphrases
below read like plain instructions rather than like search queries: the plain
register is what keeps the vocabulary disjoint.

Usage: python3 src/eval/fixtures/generate_retrieval_v1_corpus.py
"""

import json
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
OUT_CORPUS = os.path.join(ROOT, "src", "eval", "fixtures", "retrieval_v1_corpus.md")
OUT_JSON = os.path.join(ROOT, "src", "eval", "fixtures", "retrieval_v1_recall.json")

# (key, title, body). Sixty documents, one per positive case, so a random
# retriever's recall@10 is 10/60 and the number is not flattered by a small
# haystack. Bodies are deliberately ASCII-only: `query_terms` filters on BYTE
# length, and the Python replica below must agree with it exactly.
DOCS = [
    ("doc-01", "Kerning a display typeface", "Kerning adjusts pair spacing inside a display typeface so headline glyphs do not collide. Pull adjacent letters closer where the shapes leave a gap, and push them apart where strokes crowd."),
    ("doc-02", "Measuring postgres replica lag", "Replica lag measures how far a standby trails its primary, usually in bytes of write-ahead log. Watch pg_stat_replication and alert when the replay position stalls for more than a minute."),
    ("doc-03", "Feeding a sourdough starter", "A starter needs flour and water in equal weight, refreshed twice daily at room temperature. Discard half before each feed so the colony stays hungry and the rise predictable."),
    ("doc-04", "Truing a bicycle wheel", "Wheel truing corrects lateral runout by tightening and loosening spoke nipples a quarter turn at a time. Use a stand and a feeler gauge, working opposite pairs until the rim runs straight."),
    ("doc-05", "Regex catastrophic backtracking", "Nested quantifiers such as (a+)+ make the matcher explore exponential paths on a failing input. Anchor the pattern and prefer possessive or atomic groups to keep worst-case time linear."),
    ("doc-06", "Preparing an espresso puck", "Distribute the dose evenly, tamp level with about fifteen kilos, and flush the group before locking in. An uneven puck channels, so water races through one side and the shot tastes sharp."),
    ("doc-07", "Resolving git rebase conflicts", "A rebase replays commits onto a new base, stopping wherever a hunk cannot apply cleanly. Resolve each conflict, stage the file, then continue; abort restores the original branch pointer."),
    ("doc-08", "Kubernetes liveness probes", "A liveness probe restarts a container that stops responding, so it must test the process and not its dependencies. Point readiness at downstream health and keep the liveness check shallow."),
    ("doc-09", "Asset depreciation schedules", "Straight-line depreciation spreads an asset cost evenly across its useful life, while declining balance front-loads the expense. Choose the method that matches how the equipment actually loses value."),
    ("doc-10", "Rehairing a violin bow", "Rehairing replaces the horsehair ribbon, which stretches and loses grip with use. Cut the hank to length, singe the ends, and seat the plug so tension stays even across the stick."),
    ("doc-11", "Solar inverter clipping", "Clipping happens when the array output exceeds the inverter rating at midday, flattening the production curve. Size the inverter slightly below peak array watts and accept the lost harvest."),
    ("doc-12", "Splicing marine rope", "A splice braids the rope back into itself to form an eye that holds nearly full strength. Tape the ends, feed the core through the standing part, and taper the tail before whipping it."),
    ("doc-13", "Insulating a beehive for winter", "Wrap the hive with insulation board and leave a small upper vent so moisture escapes. Bees survive cold but drown in condensation, so ventilation matters more than warmth."),
    ("doc-14", "Voicing piano hammers", "Voicing shapes the tone by needling the felt where the hammer strikes the string. Soften a harsh octave with shallow needles near the crown and check the result by ear across the keyboard."),
    ("doc-15", "SQLite write-ahead log checkpoints", "The write-ahead log grows until a checkpoint copies its pages back into the main database file. Run a passive checkpoint often and a truncate checkpoint after bulk loads to reclaim disk."),
    ("doc-16", "Collimating a telescope", "Collimation aligns the primary and secondary mirrors so the optical axis stays centred. Centre the reflection of the eyepiece in the primary mirror, then fine-tune on a star at high power."),
    ("doc-17", "Sharpening on whetstones", "A whetstone grinds a fresh bevel, progressing from coarse grit to fine while the burr forms along the edge. Keep the angle steady, and finish on a strop to polish the apex."),
    ("doc-18", "Cycling nitrogen in aquaponics", "Beneficial bacteria convert fish ammonia into nitrite and then nitrate that plants absorb. Cycle the system for weeks before stocking heavily, and never let the biofilter dry out."),
    ("doc-19", "Tensioning a sailing rig", "Rig tension controls mast bend and therefore sail shape in a breeze. Tune the shrouds to a measured load, sight up the mast for a straight column, and adjust the forestay last."),
    ("doc-20", "Resoling leather boots", "Resoling replaces the worn outsole while the welt and upper stay intact. Stitch a new sole to the welt, trim the edge flush, and burnish it so the seam stays waterproof."),
    ("doc-21", "DNS zone delegation", "Delegation hands authority for a subdomain to another nameserver through NS records and a glue address. Publish the records at the parent, then verify resolution from a public resolver."),
    ("doc-22", "Choral sight reading", "Sight reading improves when singers scan the interval before sounding it and keep the pulse steady. Practise short phrases with solfege until the hand finds each note without hesitation."),
    ("doc-23", "Greenhouse humidity control", "Humidity in a greenhouse climbs when transpiration outpaces venting, encouraging mould on leaves. Open roof vents early, run a fan to stir the air, and water in the morning."),
    ("doc-24", "Reading a film camera light meter", "A light meter suggests aperture and shutter from the scene luminance and film speed. Meter the subject, then bias exposure for backlight so shadows keep their detail."),
    ("doc-25", "Laminating bread dough", "Lamination folds butter into the dough in alternating layers to build flaky structure. Roll, fold, and chill between turns so the butter stays cold and the layers stay distinct."),
    ("doc-26", "TCP retransmission timers", "A retransmission timer resends a segment when its acknowledgement never arrives, doubling the interval each attempt. Tune the initial timeout to the path round trip or the connection stalls."),
    ("doc-27", "Crazing pottery glaze", "Crazing is a network of fine cracks where the glaze shrinks more than the clay body. Adjust the glaze recipe toward more silica, or fire more slowly to relieve the stress."),
    ("doc-28", "Indexing a bicycle derailleur", "Derailleur indexing aligns the cage with each sprocket so the chain shifts cleanly. Set the high and low stops first, then dial cable tension until every gear clicks into place."),
    ("doc-29", "Cutting dovetail joints", "A dovetail joint interlocks pins and tails so the corner resists being pulled apart. Mark from the tail board, saw just outside the line, and pare to the baseline with a chisel."),
    ("doc-30", "Wine cellar temperature", "Cellar temperature should stay steady near thirteen degrees so wine ages slowly and evenly. Sudden swings push corks and oxidise the bottle, so insulate the room and monitor the humidity."),
    ("doc-31", "Tuning a hang drum", "Each note field on a hang drum is hammered to pitch, and adjacent fields share a sympathetic ring. Tap with the pad of a finger and check the octave overtone above each tone."),
    ("doc-32", "Inoculating mushroom logs", "Drill plug holes around a fresh hardwood log, tap in spawn, and seal with wax so the mycelium colonises. Shade the log and keep it damp for a full season before fruiting."),
    ("doc-33", "Replacing cello strings", "Replace one string at a time so the bridge and soundpost stay put. Wind the peg toward the scroll, stretch the new string gently, and retune over several days as it settles."),
    ("doc-34", "Tuning a drone flight controller", "Proportional, integral, and derivative gains shape how a quadcopter reacts to disturbance. Raise proportional until oscillation appears, then add derivative damping and integral trim."),
    ("doc-35", "Coptic bookbinding stitch", "The coptic stitch links each signature to the boards with a chain of thread along the spine. Sew through the fold, loop around the previous station, and pull gently to keep tension."),
    ("doc-36", "Adjusting aquarium water hardness", "General hardness measures dissolved calcium and magnesium, which buffer the pH. Test the tank, then blend reverse-osmosis water or add minerals until the species suits the reading."),
    ("doc-37", "Building a climbing anchor", "A sound anchor equalises load across several protection points with a cordelette or sling. Keep the angles under sixty degrees so each piece shares the force rather than multiplying it."),
    ("doc-38", "Letterpress ink consistency", "Letterpress ink must stay tacky enough to transfer yet loose enough to release the type. Add drier sparingly and roll the plate evenly, watching for squash and mottling."),
    ("doc-39", "Checking bicycle chain wear", "A stretched chain wears the sprockets, so measure elongation with a gauge every few hundred kilometres. Replace the chain at half a percent stretch to spare the cassette."),
    ("doc-40", "Maintaining a septic tank", "A septic tank separates solids while bacteria digest the effluent in the drain field. Pump the tank every few years and never flush solvents that kill the bacterial mat."),
    ("doc-41", "Knitting gauge swatches", "A gauge swatch tells you how many stitches and rows fill a measured square. Block the swatch before counting, and change needle size until the fabric matches the pattern tension."),
    ("doc-42", "Seasoning cast iron", "Seasoning builds a polymerised oil layer that makes cast iron non-stick and rust resistant. Coat thinly, bake hot, and repeat; wash with water only, never with harsh soap."),
    ("doc-43", "Designing API rate limits", "A rate limit protects a service by capping requests per client window, usually with a token bucket. Return the remaining quota in headers and a retry hint when the client is throttled."),
    ("doc-44", "Chess endgame tablebases", "Tablebases store the exact result of every legal position with few pieces remaining. Consult one to convert a winning pawn ending and to avoid a drawn fortress."),
    ("doc-45", "Capturing a bee swarm", "A swarm clusters on a branch while scouts search for a new cavity. Shake the cluster into a box, leave it until dusk, and move it once the foragers have returned."),
    ("doc-46", "Dyeing leather", "Leather dye penetrates the grain and settles into the fibres, so apply thin coats and let each dry. Seal with a finish coat or the colour will crock onto clothing."),
    ("doc-47", "Turning a compost pile", "Turning a compost pile aerates the core so aerobic microbes keep working fast. Mix the outer layer inward, keep it as damp as a wrung sponge, and turn weekly."),
    ("doc-48", "Developing film temperature", "Film development depends on developer temperature, which sets contrast and grain. Hold the tank near twenty degrees in a water bath and agitate on a fixed schedule."),
    ("doc-49", "TIG welding aluminium", "Aluminium conducts heat away fast and oxidises instantly, so clean the metal and use alternating current. Set the balance toward cleaning and move quickly to avoid a sagging puddle."),
    ("doc-50", "Setting up a ukulele", "Action height at the nut and saddle decides how hard a ukulele is to fret. Lower the saddle gradually, check intonation at the twelfth fret, and file the nut slots."),
    ("doc-51", "Docker layer caching", "Docker caches each build layer keyed by its instruction and the files it copies. Order the Dockerfile so dependencies install before source code, and the cache survives edits."),
    ("doc-52", "Scoring bread dough", "Scoring controls where a loaf expands in the oven, so a decisive slash lets steam escape. Angle the blade shallow, cut quickly, and dust flour so the pattern stays visible."),
    ("doc-53", "Adjusting a motorcycle chain", "Motorcycle chain slack should sit within the manual range at the tightest point. Loosen the axle, turn the adjusters evenly, and check alignment with the marks or a straight edge."),
    ("doc-54", "Cleaning a bird feeder", "A dirty feeder spreads disease among flocking birds, so scrub it with hot soapy water every fortnight. Rinse well, dry fully, and refresh the seed before it clumps."),
    ("doc-55", "Grouting floor tiles", "Grout fills the joints between tiles and locks them against movement. Press it into the gaps with a float, wipe the haze before it sets, and seal once cured."),
    ("doc-56", "Harvesting rainwater", "Rainwater harvesting channels roof runoff into a tank through a first-flush diverter that discards the dirty opening flow. Filter the inlet and screen the overflow."),
    ("doc-57", "Sizing a solar battery bank", "Battery capacity is sized from nightly consumption divided by usable depth of discharge. Add a margin for cloudy days and match the bank voltage to the inverter."),
    ("doc-58", "Managing a poker bankroll", "Bankroll management keeps stakes small relative to the money set aside, so variance cannot ruin you. Move up only when the roll supports the bigger game."),
    ("doc-59", "Tuning nginx upstream timeouts", "Upstream timeouts bound how long nginx waits for a backend response before failing over. Set them above the slowest healthy request and below the client patience limit."),
    ("doc-60", "Grinding lathe tool bits", "A lathe tool bit is ground with rake and relief angles that suit the material being cut. Hold the blank against the wheel, cool it often, and hone the edge on a stone."),
]

# Lexical: each query reuses rare vocabulary from its gold body, so BM25 has a
# literal hook. The validator additionally requires at least one shared term
# with a document frequency of two or less -- a query that only shares `the`
# with its gold is not a lexical case in any useful sense.
LEXICAL = [
    ("doc-01", "kerning pair spacing display typeface"),
    ("doc-02", "postgres replica lag standby write-ahead log"),
    ("doc-03", "sourdough starter flour water refresh discard"),
    ("doc-04", "bicycle wheel truing spoke nipples lateral runout"),
    ("doc-05", "regex catastrophic backtracking nested quantifiers"),
    ("doc-06", "espresso puck tamp channeling dose"),
    ("doc-07", "git rebase conflicts hunk continue abort"),
    ("doc-08", "kubernetes liveness probe container restart"),
    ("doc-09", "depreciation schedule straight-line declining balance asset"),
    ("doc-10", "violin bow rehairing horsehair ribbon plug"),
    ("doc-11", "solar inverter clipping array peak watts"),
    ("doc-12", "marine rope splice braid eye tail"),
    ("doc-13", "beehive insulation board condensation vent winter"),
    ("doc-14", "piano hammer voicing needle felt tone"),
    ("doc-15", "sqlite write-ahead log checkpoint truncate pages"),
    ("doc-16", "telescope collimation primary secondary mirror optical axis"),
    ("doc-17", "whetstone sharpening bevel burr strop grit"),
    ("doc-18", "aquaponics nitrogen cycle bacteria ammonia nitrate biofilter"),
    ("doc-19", "sailing rig tension shrouds mast bend forestay"),
    ("doc-20", "leather boots resoling welt outsole stitch"),
    ("doc-21", "dns zone delegation nameserver glue records"),
    ("doc-22", "choral sight reading solfege interval pulse"),
    ("doc-23", "greenhouse humidity transpiration roof vents fan"),
    ("doc-24", "film camera light meter aperture shutter luminance"),
    ("doc-25", "bread lamination folds butter layers chill"),
    ("doc-26", "tcp retransmission timer segment acknowledgement doubling"),
    ("doc-27", "pottery glaze crazing silica stress cracks"),
    ("doc-28", "derailleur indexing cage sprocket stops cable tension"),
    ("doc-29", "dovetail joint pins tails baseline chisel"),
    ("doc-30", "wine cellar temperature cork oxidise humidity"),
]

# Paraphrase: the query states the same need in a plain register that shares no
# term at all with the gold body. The validator proves the disjointness; a
# single shared three-letter word turns the case lexical and fails the build.
PARAPHRASE = [
    ("doc-31", "how do I get a metal bowl to sound right"),
    ("doc-32", "how do I get mushrooms to grow on a piece of timber"),
    ("doc-33", "how do I swap a wire without moving wooden parts"),
    ("doc-34", "why does my four rotor craft wobble"),
    ("doc-35", "how do I hold paper sheets together by hand"),
    ("doc-36", "why do my fish suffer in soft tap fluid"),
    ("doc-37", "how do I spread pull between two bolts"),
    ("doc-38", "why is my printed page blotchy"),
    ("doc-39", "how do I know my bike drive is worn out"),
    ("doc-40", "how often should a sewage pit be emptied"),
    ("doc-41", "why does my wool end up wrong width"),
    ("doc-42", "how do I keep a black skillet from going sticky"),
    ("doc-43", "how do I stop one caller flooding my server"),
    ("doc-44", "how do I force a mate using a lone king"),
    ("doc-45", "how do I collect a cloud of insects hanging from a tree"),
    ("doc-46", "how do I tint a hide without it rubbing off"),
    ("doc-47", "how do I get a heap of scraps to rot quicker"),
    ("doc-48", "why do my negatives come out too dark"),
    ("doc-49", "how do I join thin alloy sheet with an arc"),
    ("doc-50", "why does my small guitar hurt my fingers"),
    ("doc-51", "why does a rebuild take so long every time"),
    ("doc-52", "how do I make a gash on top before baking"),
    ("doc-53", "how much play does a bike drive need"),
    ("doc-54", "how do I keep a hanging tray clean of germs"),
    ("doc-55", "how do I plug spaces in a wet room"),
    ("doc-56", "how do I store what pours off my house"),
    ("doc-57", "how big should my storage be to last overnight"),
    ("doc-58", "how much cash should I risk at cards"),
    ("doc-59", "why does my proxy give up too early"),
    ("doc-60", "how do I shape a cutter for steel"),
]

# Negatives: topics no document covers. An empty gold set is the label that
# says retrieval should stay silent (#996), and it is the only way the
# false-positive rate can be measured at all.
NEGATIVES = [
    "how do I install a chimney flue liner",
    "how do I prune an old apple orchard",
    "how do I calibrate a metal detector",
    "how do I replace a cracked windscreen",
    "what makes a washing machine drum squeal",
    "how do I train a puppy to sit",
    "how do I set up a home cinema projector",
    "how do I stop a radiator valve leaking",
    "how do I preserve tomatoes under oil",
    "how do I lay a garden brick wall",
    "how do I install a loft ladder",
    "how do I build a raised vegetable bed",
]


def query_terms(text):
    """Byte-for-byte the shipped `brain_sections::query_terms`.

    Rust: split on whitespace, trim non-alphanumerics from both ends, lowercase,
    keep terms whose BYTE length exceeds two. No stopword list, which is why
    `the` and `and` count as content terms and why the paraphrases avoid them.
    """
    out = []
    for raw in text.split():
        start, end = 0, len(raw)
        while start < end and not raw[start].isalnum():
            start += 1
        while end > start and not raw[end - 1].isalnum():
            end -= 1
        term = raw[start:end].lower()
        if len(term.encode("utf-8")) > 2:
            out.append(term)
    return out


def doc_id(key, title):
    """The section id IS the heading text, which is also the store's doc key.

    One identity across the corpus, the labels, the BM25 ranker and the store,
    so a gold label can never resolve in one and miss in the other.
    """
    return f"{key} - {title}"


def build():
    by_key = {key: (title, body) for key, title, body in DOCS}
    if len(by_key) != len(DOCS):
        sys.exit("duplicate document key")

    ids = [doc_id(key, title) for key, title, _ in DOCS]
    if len(ids) != len(set(ids)):
        sys.exit("duplicate section ids")

    for text in [t for _, t, _ in DOCS] + [b for _, _, b in DOCS]:
        if not text.isascii():
            sys.exit(f"non-ASCII text would break the byte-length replica: {text!r}")

    # Document frequency of every term, over the whole corpus.
    df = {}
    for key, title, body in DOCS:
        seen = set(query_terms(f"{doc_id(key, title)} {body}"))
        for term in seen:
            df[term] = df.get(term, 0) + 1

    cases = []
    for key, query in LEXICAL:
        title, body = by_key[key]
        shared = set(query_terms(query)) & set(query_terms(f"{doc_id(key, title)} {body}"))
        if not shared:
            sys.exit(f"lexical case for {key} shares no term with its gold: {query!r}")
        rarest = min(df[t] for t in shared)
        if rarest > 2:
            sys.exit(
                f"lexical case for {key} only shares common terms "
                f"({sorted(shared)}), so BM25 has no literal hook: {query!r}"
            )
        cases.append({"query": query, "relevant": [doc_id(key, title)]})

    for key, query in PARAPHRASE:
        title, body = by_key[key]
        shared = set(query_terms(query)) & set(query_terms(f"{doc_id(key, title)} {body}"))
        if shared:
            sys.exit(
                f"paraphrase case for {key} shares {sorted(shared)} with its gold, "
                f"so it would be classed lexical: {query!r}"
            )
        cases.append({"query": query, "relevant": [doc_id(key, title)]})

    for query in NEGATIVES:
        cases.append({"query": query, "relevant": []})

    # The split the Rust test re-derives and asserts. Kept here so the two
    # implementations have to agree: a Python/Rust disagreement is a loud
    # failure rather than a silently mislabelled corpus.
    lexical = sum(1 for c in cases if c["relevant"])
    negatives = sum(1 for c in cases if not c["relevant"])
    total = len(cases)
    if total < 60:
        sys.exit(f"only {total} cases, the design requires at least 60")
    if len(PARAPHRASE) < 25:
        sys.exit(f"only {len(PARAPHRASE)} paraphrase cases, at least 25 required")
    if negatives < 10:
        sys.exit(f"only {negatives} negatives, at least 10 required")

    corpus = ["# Retrieval v1 - ablation corpus", ""]
    corpus.append(
        "Generated by `src/eval/fixtures/generate_retrieval_v1_corpus.py` from "
        "the document table in that script - do not edit by hand. Each heading "
        "is a document id, and the same string is the store key the eval harness "
        "indexes under, so a label resolves identically in the BM25 leg and the "
        "vector leg."
    )
    corpus.append("")
    for key, title, body in DOCS:
        corpus.append(f"## {doc_id(key, title)}")
        corpus.append("")
        corpus.append(body)
        corpus.append("")

    with open(OUT_CORPUS, "w", encoding="utf-8") as handle:
        handle.write("\n".join(corpus))

    payload = {
        "name": "retrieval-v1",
        "note": (
            "Class labels are NOT stored: the loader derives lexical vs "
            "paraphrase from the shipped tokenizer, so a mislabelled case "
            "cannot flatter the vector side (#981)."
        ),
        "cases": cases,
    }
    with open(OUT_JSON, "w", encoding="utf-8") as handle:
        json.dump(payload, handle, indent=2)
        handle.write("\n")

    print(f"documents: {len(DOCS)}")
    print(f"cases: {total} (lexical {lexical - len(PARAPHRASE)} / paraphrase {len(PARAPHRASE)} / negative {negatives})")
    print(f"corpus: {os.path.getsize(OUT_CORPUS)} bytes -> {OUT_CORPUS}")
    print(f"labels: {os.path.getsize(OUT_JSON)} bytes -> {OUT_JSON}")


if __name__ == "__main__":
    build()
