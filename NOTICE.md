# Licensing

This repository has two parts with two licenses.

| Part | License | Why |
|---|---|---|
| `server/`, `bridge/`, `scripts/`, `tests/`, `docs/`, this README | [MIT](LICENSE) | Original code that does not contain any RapidRAW code. |
| `editor/` (the files added to RapidRAW and the patch that edits it) | [AGPL-3.0](editor/LICENSE) | It is part of RapidRAW once applied, and RapidRAW is AGPL-3.0. |

RapidRAW is written by CyberTimon and contributors and published at
<https://github.com/CyberTimon/RapidRAW> under the GNU Affero General Public License v3.0. A RapidRAW
build that includes the `editor/` changes is a modified version of RapidRAW and must be distributed under
that license, source included, like any other modified RapidRAW. That obligation cannot be waived by this
repository, so the `editor/` folder carries the AGPL text and is not MIT.

The files in `bridge/` are MIT-licensed and are compiled into both the MIT server and the AGPL editor.
MIT code can be used inside an AGPL work, so there is no conflict: the server stays MIT, the modified
editor as a whole is AGPL.

This project is not affiliated with or endorsed by the RapidRAW project.
