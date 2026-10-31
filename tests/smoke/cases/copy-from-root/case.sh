# `COPY --from=<stage> /`: the whole stage, which kaniko mode keeps by its
# sources before the root is reused, the state directory, /ranikust and
# /proc left out. Red-team finding: in kaniko mode on the container's `/`
# the keep refuses the root itself ("a path to keep lies on another
# mount", `/` being a mount root), so the build fails there.
case_run() {
  build img >/dev/null
  expect "$(run img)" "ok" "checks inside the image"
}
