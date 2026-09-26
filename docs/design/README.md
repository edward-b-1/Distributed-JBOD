# Design comparisons for the web UI

Each page here was made to choose between treatments of one part of the
console, with the options drawn side by side in the console's own colours,
light and dark. They are kept so the decision and the alternatives it beat
are on record. Open any of them in a browser; they are self-contained.

| Page | Question | Decided |
|---|---|---|
| [navigation.html](navigation.html) | How the sections are presented: segmented control, underline, pill, folder tabs, header line, sidebar, or quiet text with counts. | **F, the sidebar** with an icon and a live count per section (PR #64), later made collapsible (PR #90). |
| [agreement-indicator.html](agreement-indicator.html) | How the page says whether every node holds the same cluster document: a pill, a dot, a status strip, a chip per node, a health card, or a fraction ring. | **E, the health card** at the top of the Overview (PR #83). |
| [health-drill-down.html](health-drill-down.html) | How to get from the card to the state of each node: the card unfolding to chips, to node cards with device blocks, to a checks table, or a Nodes page. | None of these as drawn: a **Nodes tab** holding one nodes table with every check folded in (PRs #89, #92). |
| [object-health.html](object-health.html) | How the console says how many objects exist and whether each can be read: a second health card, a segmented bar, or tiles on the Overview; the four states the count can be in; a health filter and Shards column on the Objects page; a verdict line in the record view. | Not yet taken. Recommended: the record-view verdict first (UI only), then the health card as **A** once the node has a records-only summary operation (#209, #210). |
| [status-marks.html](status-marks.html) | One family of pass, fail, warning, and off marks: filled circles, outlines, rounded squares, bare glyphs, or shapes. | **E, shapes**: dot, square, triangle, ring (PR #96). |

The pages are hand-written HTML with no build step and no dependencies.
When a new comparison is made, add it here with the question and the
decision, once taken.
