//! Read-only command policies. Scheduling capability is independent of approval.
//! CCB BashTool/readOnlyValidation and shell/readOnlyCommandValidation are the
//! behavioral references; only validated argument forms may overlap.
use super::flags::Flags;

pub(super) fn branch(args: &[String]) -> bool {
    let flags = Flags {
        switches: "-l --list -a --all -r --remotes -v --verbose --no-color --no-column --no-abbrev --show-current -i --ignore-case",
        values: "--contains --no-contains --points-at --sort",
        attached: "--color --column --abbrev --merged --no-merged",
    };
    flags.parse(args).is_some_and(|p| {
        p.positional.is_empty()
            || p.flags
                .iter()
                .any(|f| f == "--list" || f == "-l" || f == "--merged" || f == "--no-merged")
    })
}

pub(super) fn fd(args: &[String]) -> bool {
    Flags {
        switches: "-h --help -V --version -H --hidden -I --no-ignore --no-ignore-vcs --no-ignore-parent -s --case-sensitive -i --ignore-case -g --glob --regex -F --fixed-strings -a --absolute-path -L --follow -p --full-path -0 --print0 --strip-cwd-prefix --show-errors --one-file-system --no-require-git --exactly --quiet -q",
        values: "-d --max-depth --min-depth --exact-depth -t --type -e --extension -S --size --changed-within --changed-before -o --owner -E --exclude --ignore-file -c --color -j --threads --max-results --path-separator --search-path --base-directory",
        attached: "",
    }.parse(args).is_some()
}

pub(super) fn diff(args: &[String]) -> bool {
    Flags {
        switches: "-q --brief -s --report-identical-files -c -u -e --ed -n --rcs -y --side-by-side --left-column --suppress-common-lines -p --show-c-function -t --expand-tabs -T --initial-tab --suppress-blank-empty -r --recursive -N --new-file --unidirectional-new-file --ignore-file-name-case --no-ignore-file-name-case -a --text --strip-trailing-cr -i --ignore-case -E --ignore-tab-expansion -Z --ignore-trailing-space -b --ignore-space-change -w --ignore-all-space -B --ignore-blank-lines -d --minimal --speed-large-files --help -v --version",
        values: "-C -U -W --width --tabsize -F --show-function-line --label -x --exclude -X --exclude-from -S --starting-file --from-file --to-file -I --ignore-matching-lines --horizon-lines",
        attached: "--color --context --unified",
    }.parse(args).is_some()
}

pub(super) fn gh(args: &[String]) -> bool {
    let [group, command, rest @ ..] = args else {
        return false;
    };
    let flags = match (group.as_str(), command.as_str()) {
        ("pr" | "issue", "list") => Flags {
            switches: "--draft --help",
            values: "--state -s --author --assignee --label --limit -L --base --head --search --json --app --repo -R --milestone",
            attached: "",
        },
        ("pr" | "issue", "view") => Flags { switches: "--comments --help", values: "--json --repo -R", attached: "" },
        ("pr", "diff") => Flags { switches: "--name-only --patch --help", values: "--color --repo -R", attached: "" },
        ("pr", "checks") => Flags { switches: "--watch --required --fail-fast --help", values: "--json --interval --repo -R", attached: "" },
        ("repo", "view") => Flags { switches: "--help", values: "--json --branch -b", attached: "" },
        _ => return false,
    };
    flags.parse(rest).is_some()
}

pub(super) fn accepts_globs(command: &str) -> bool {
    matches!(
        command,
        "ls" | "cat"
            | "head"
            | "tail"
            | "wc"
            | "stat"
            | "grep"
            | "egrep"
            | "fgrep"
            | "diff"
            | "du"
            | "df"
            | "echo"
            | "strings"
            | "hexdump"
            | "od"
            | "nl"
            | "cut"
            | "column"
            | "tr"
            | "tac"
            | "rev"
            | "cmp"
            | "basename"
            | "dirname"
            | "realpath"
            | "readlink"
            | "sha256sum"
            | "sha1sum"
            | "md5sum"
            | "cd"
    )
}
