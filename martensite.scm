;;; martensite.scm
;;; Steel plugin for Helix: send Julia code to a live REPL served by
;;; JuliaDaemon.jl (`jld`).
;;;
;;; Requires `jld` on PATH:
;;;   julia -e 'using Pkg; Pkg.app add url="https://github.com/KristofferC/JuliaDaemon.jl"'
;;;
;;; On the Julia side, either topology works — both must name the session, and
;;; the name has to match *resolve-session* below (default "repl"):
;;;
;;;   A. daemon owns Main, thin REPL attached:  jld connect --name=repl
;;;   B. your own julia serves itself:          using JuliaDaemon
;;;                                             JuliaDaemon.serve(name="repl")
;;;
;;; Topology A survives closing the terminal; B dies with it. Agents reach the
;;; same session with `jld --id=<id> eval` (captured output, dev's prompt
;;; untouched), `--scratch` for a throwaway module that can't clobber Main's
;;; bindings, and `jld transcript` to read what the developer has been doing.

(require "helix/misc.scm")        ; set-status!, set-warning!, set-error!, cursor-position, push-component!, pop-last-component-by-name!
(require "helix/static.scm")      ; current-selection->string, get-helix-cwd
(require "helix/editor.scm")      ; set-register!, editor-focus, editor->doc-id, editor->text
(require "helix/ext.scm")         ; hx.with-context, spawn-native-thread
(require "helix/treesitter.scm")  ; document->tree, tstree->root, tsnode-*
(require "helix/components.scm")  ; new-component!, block/render, buffer/clear, Color/*, style*, frame-set-string!

(require-builtin steel/process)      ; command, with-stdout-piped, with-stderr-piped, spawn-process, child-stdout, child-stderr, wait
(require-builtin steel/ports)        ; read-port-to-string, open-input-file, read-line-from-port
(require-builtin steel/meta)         ; maybe-get-env-var
(require-builtin helix/core/text)    ; rope-char->byte, rope->byte-slice, rope->string
(require "steel/result")             ; Ok?, unwrap-ok

;; VTE (terminal emulator) support for rendering ANSI-colored output in the
;; popup — already installed for this Helix config's own term.scm.
;;
;; NOTE (jld port): `jld eval` currently emits *no* ANSI color — the wire
;; protocol carries a `color` field (daemon.jl:90, read at :622) but only
;; connect_repl.jl ever sets it true, so the CLI path is always monochrome and
;; there is no flag to change that. The VTE is kept anyway: it costs nothing,
;; still does the line-wrapping this popup relies on, and lights up for free if
;; a `--color` flag lands upstream.
(#%require-dylib "libsteel_pty"
                 (only-in raw-virtual-terminal
                          vte/resize
                          vte/advance-bytes
                          vte/reset-iterator!
                          vte/advance-iterator!
                          vte/iter-x
                          vte/iter-y
                          vte/iter-cell-fg
                          vte/iter-cell-bg
                          vte/iter-cell-str
                          term/color-attribute))

(provide send-to-julia-repl)
(provide send-top-level-to-julia-repl)
(provide eval-in-julia)
(provide eval-top-level-in-julia)

;; ─── Session resolution ──────────────────────────────────────────────────────
;; Resolves a *name*, not a socket: `jld` already keys a daemon on the project
;; (the nearest Project.toml walking up from Helix's cwd), so the name only has
;; to disambiguate several REPLs on one project.
;;
;; Every source here is read from the environment or from a file, deliberately.
;; An earlier version also probed `zellij action current-tab-info` and
;; `tmux display-message`, mirroring the old quench.sh. That had to go: the
;; plugin and the REPL probe independently, so one side can succeed while the
;; other fails, and the two then derive different daemon ids with nothing to
;; indicate it. That is not hypothetical — it is exactly how this broke in
;; practice, with the REPL falling back to "repl" while the plugin resolved the
;; tab name. `zellij action` also needs ZELLIJ_SESSION_NAME and, without it,
;; prints a session-picker message to *stdout* rather than failing, so a naive
;; caller parses that as a tab name. And with every tab named "Tab #1" by
;; default, the probe contributed no disambiguation to pay for the risk.
;;
;; "repl" is the default on purpose — it is what JuliaDaemon.serve() picks when
;; called with no arguments, so a self-serving session needs zero configuration.
;; Note that a *spawned* daemon's default name is "" (client.jl make_ctx), which
;; is NOT the same id, so the name is always passed explicitly rather than
;; omitted.

(define *default-session-name* "repl")

(define (env-or-false name)
  (define r (maybe-get-env-var name))
  (if (and (Ok? r) (not (equal? (unwrap-ok r) ""))) (unwrap-ok r) #f))

;; First line of a file, trimmed; #f if unreadable or blank.
(define (first-line path)
  (with-handler
    (lambda (e) #f)
    (let ([line (read-line-from-port (open-input-file path))])
      (if (string? line)
          (let ([t (trim line)]) (if (equal? t "") #f t))
          #f))))

;; `wait` hands back a Steel Result wrapping the exit status — NOT a bare int
;; (verified: it prints as `(Ok 0)`), so comparing it to 0 directly is silently
;; always false. Unwrap it, and treat an Err (process died without a code, e.g.
;; killed by a signal) as a nonzero exit.
(define (exit-code proc)
  (define r (wait proc))
  (if (Ok? r) (unwrap-ok r) -1))

;; jld sanitizes session names in two different places and at two different
;; times: `serve_session` (daemon.jl) replaces [^A-Za-z0-9_.-] with "-" and then
;; hashes the SANITIZED name, while `make_ctx` (client.jl) hashes the name it was
;; given, raw. A name with a space in it therefore yields two different daemon
;; ids and the two sides never meet — and Zellij's default tab name is "Tab #1".
;;
;; Sanitizing here, before the name is passed to either, makes both hash the same
;; string. Strict ASCII on purpose: char-alphabetic? would keep "é", which Julia's
;; [A-Za-z] does not. Verified character-for-character against Julia, multi-byte
;; input included.
(define (name-safe-char? c)
  (define n (char->integer c))
  (or (and (>= n 48) (<= n 57))                       ; 0-9
      (and (>= n 65) (<= n 90))                       ; A-Z
      (and (>= n 97) (<= n 122))                      ; a-z
      (equal? n 45) (equal? n 46) (equal? n 95)))     ; - . _

(define (sanitize-name s)
  (list->string (map (lambda (c) (if (name-safe-char? c) c #\-)) (string->list s))))

(define (resolve-session)
  (sanitize-name (resolve-session-raw)))

(define (resolve-session-raw)
  (or (env-or-false "MARTENSITE_SESSION")
      ;; jld's own override, honored so that one variable set in a Zellij/tmux
      ;; layout names the session on both sides at once — the plugin reads it
      ;; here, `jld connect` reads it from make_ctx.
      (env-or-false "JLD_NAME")
      (first-line ".juliasession")
      *default-session-name*))

;; ─── jld invocation ──────────────────────────────────────────────────────────
;; Both stdout and stderr are piped and captured — Julia writes errors and
;; stacktraces to stderr, and an unpiped stderr is inherited from Helix's own
;; process: it writes raw text straight to the terminal, bypassing the TUI
;; compositor (and this popup) entirely.
;;
;; wait->stdout can't be combined with child-stderr: internally it takes the
;; whole child handle (Rust's Child::wait_with_output, which reads both but
;; hands back only stdout), so by the time it returns there is no child left to
;; pull stderr from. Grab both port handles before waiting, wait separately
;; after.
;;
;; No --project is passed: jld's find_project walks *up* from the subprocess
;; cwd (inherited from Helix), whereas an explicit --project=DIR demands a
;; Project.toml at exactly that directory and dies otherwise.
(struct JldResult (code output))

(define (run-jld args)
  (define proc
    (~> (command "jld" args)
        with-stdout-piped
        with-stderr-piped
        spawn-process
        unwrap-ok))
  (define out (read-port-to-string (child-stdout proc)))
  (define err (read-port-to-string (child-stderr proc)))
  (define code (exit-code proc))
  (JldResult code (string-append out err)))

(define (name-flag) (string-append "--name=" (resolve-session)))

;; Captured eval: output comes back to us, the developer's prompt is never
;; touched, `ans` is not set. --max-output caps a runaway print loop so it
;; cannot swamp the popup.
(define (jld-eval code)
  (run-jld (list (name-flag) "--max-output=16k" "eval" code)))

;; Paste into the live prompt: bracketed-paste injection into the REPL's tty
;; buffer (repl_input.jl), so it is echoed at the prompt, evaluated by the REPL
;; itself and sets `ans`, with any half-typed input stashed and restored.
;;
;; The catch, and the one real regression against `temper --sync --print`:
;; cmd_eval_repl (client.jl:1104) reads back only a `done` frame, so the result
;; lands in the developer's terminal and never reaches us. Nothing to popup.
(define (jld-eval-repl code)
  (run-jld (list (name-flag) "eval-repl" code)))

;; ─── Output popup ───────────────────────────────────────────────────────────
;; A custom component (new-component!) that draws a bordered block, then
;; renders captured output through a real VTE (steel-pty's
;; raw-virtual-terminal) rather than plain fenced markdown, so the VTE does its
;; own line-wrapping (and would interpret SGR codes if any arrived).
(struct OutputPopup (vte width height row col border-style title overflow))

(define (clamp lo hi v)
  (max lo (min hi v)))

(define *popup-max-width* 60)
(define *popup-max-height* 10)
;; Floor on the *inner* width so a two-character result still gets a box wide
;; enough for its title rather than a sliver.
(define *popup-min-inner-width* 16)

;; The VTE's actual row count — much taller than the popup ever shows, so a
;; long stacktrace doesn't scroll its own beginning off the top before render
;; time. See show-output!.
(define *popup-vte-scrollback-rows* 300)

;; term/color-attribute converts an opaque TermColorAttribute into (list r g b
;; a), an indexed-palette int, or #false for "use the theme default". There is
;; no functional "build me an indexed Color" constructor — mirrors steel-pty's
;; own term.scm, which mutates a scratch Color in place instead.
(define (attr->color attr)
  (cond
    [(list? attr)
     (define c (Color/rgb (car attr) (cadr attr) (caddr attr)))
     c]
    [(int? attr)
     (define c (Color/rgb 0 0 0))
     (set-color-indexed! c attr)
     c]
    [else #f]))

;; vte/iter-cell-fg and vte/iter-cell-bg return a raw #false — not a
;; TermColorAttribute — whenever the iterator has no "last cell" set yet;
;; term/color-attribute only accepts the opaque type, so it must never be
;; called on that #false.
(define (cell-color-attr raw)
  (if raw (term/color-attribute raw) #f))

;; Builds a fresh Style per cell rather than reusing/defaulting one: there is
;; no way to pull a bare Color? back out of the theme lookups (they return a
;; whole Style?, which set-style-fg!/set-style-bg! choke on). Leaving fg/bg
;; unset on a Default-attribute cell means "don't touch what's already there" —
;; which is the block's theme colors that block/render already painted
;; underneath.
(define (cell-style vte)
  (define s (style))
  (define fg (attr->color (cell-color-attr (vte/iter-cell-fg vte))))
  (define bg (attr->color (cell-color-attr (vte/iter-cell-bg vte))))
  (when fg (set-style-fg! s fg))
  (when bg (set-style-bg! s bg))
  s)

;; True rendered extent of what the VTE holds, as (width . height).
;;
;; This has to run AFTER the bytes are fed: the VTE applies its own wrapping, so
;; its cell grid is the only accurate measure of how much space the content
;; needs. Measuring the raw string instead would miss wrapping entirely and get
;; long lines badly wrong — which is why the box used to be a fixed size.
;;
;; vte/iter-x is 1-indexed, so the maximum x IS the column count; vte/iter-y is
;; 0-indexed, hence the +1. Blank cells are skipped so trailing padding and the
;; unused tail of the 300-row scrollback don't inflate the result.
(define (vte-extent vte)
  (vte/reset-iterator! vte)
  (let loop ([w 0] [h 0])
    (if (vte/advance-iterator! vte)
        (let ([cell (vte/iter-cell-str vte)])
          (if (and (string? cell) (not (equal? (trim cell) "")))
              (loop (max w (vte/iter-x vte)) (max h (+ 1 (vte/iter-y vte))))
              (loop w h)))
        (cons w h))))

;; Writes a short label into a border row, clipped to stay inside the corners.
(define (draw-badge! frame x y max-w text style)
  (define room (- max-w 4))
  (when (> room 0)
    (frame-set-string! frame x y
                       (if (> (string-length text) room)
                           (substring text 0 room)
                           text)
                       style)))

(define (output-popup-render state rect frame)
  (define w (min (OutputPopup-width state) (area-width rect)))
  (define h (min (OutputPopup-height state) (area-height rect)))
  (define max-x (max (area-x rect) (- (+ (area-x rect) (area-width rect)) w)))
  (define max-y (max (area-y rect) (- (+ (area-y rect) (area-height rect)) h)))
  (define x (clamp (area-x rect) max-x (OutputPopup-col state)))
  (define y (clamp (area-y rect) max-y (OutputPopup-row state)))
  (define box (area x y w h))
  (define border (OutputPopup-border-style state))
  (buffer/clear frame box)
  (block/render frame box (make-block (theme-scope "ui.background") border "all" "rounded"))
  ;; Title and overflow badge are drawn INTO the border row: make-block takes
  ;; only (style border-style borders border-type) and has no title support, so
  ;; overwriting the border cells is the way to label a box.
  (draw-badge! frame (+ x 2) y w (OutputPopup-title state) (style-with-bold border))
  (let ([hidden (OutputPopup-overflow state)])
    (when (> hidden 0)
      (define badge (string-append " ⋯ +" (int->string hidden) " more "))
      (draw-badge! frame
                   (max (+ x 2) (- (+ x w) (string-length badge) 2))
                   (- (+ y h) 1)
                   w badge border)))
  ;; +2, not +1: 1 cell for the border plus 1 cell of padding, so content
  ;; doesn't render flush against the border wall.
  (define inner-x (+ 2 x))
  (define inner-y (+ 2 y))
  ;; Window into the VTE's tall scrollback: only the rows that fit the padded
  ;; interior.
  (define visible-rows (- h 4))
  (define vte (OutputPopup-vte state))
  (vte/reset-iterator! vte)
  (let loop ()
    (when (vte/advance-iterator! vte)
      ;; vte/iter-cell-str, like the fg/bg accessors, returns raw #false
      ;; whenever the iterator has no "last cell" set yet — skip those rather
      ;; than handing #false to frame-set-string!.
      (define cell-str (vte/iter-cell-str vte))
      (when (and (string? cell-str) (< (vte/iter-y vte) visible-rows))
        (frame-set-string! frame
                           ;; vte/iter-x is 1-indexed (first column reports as
                           ;; 1, not 0) — verified against steel-pty directly;
                           ;; vte/iter-y is 0-indexed, no adjustment needed.
                           (+ inner-x (- (vte/iter-x vte) 1))
                           (+ inner-y (vte/iter-y vte))
                           cell-str
                           (cell-style vte)))
      (loop))))

;; Dismiss on any keypress, mirroring the built-in doc-popup behavior.
;; handle_event fires for every event (redraws, resizes, etc.), not just key
;; presses, so closing unconditionally closed the popup on the very next
;; non-key tick — check key-event? first.
(define (output-popup-handle-event state event)
  (if (key-event? event) event-result/close event-result/ignore))

;; Show output in a floating, bordered popup anchored just below the cursor,
;; sized to what the content actually needs.
(define (show-output! output error?)
  ;; Replace any popup from a still-open previous call rather than stacking a
  ;; new one on top of it.
  (pop-last-component-by-name! "martensite-output")
  (define vte (raw-virtual-terminal))
  ;; Rows: far taller than the popup ever shows, windowed down at render time. A
  ;; VTE sized to exactly the visible rows scrolls like a real terminal as it is
  ;; fed — showing only the *tail* of a long stacktrace and cutting off the
  ;; "ERROR: ..." message at the top, which is the useful part.
  ;;
  ;; Cols: -4 for border (2) + padding (2), matching inner-x/inner-y in the
  ;; renderer — otherwise the VTE wraps wider than the interior has room for.
  (define max-inner-w (- *popup-max-width* 4))
  (define max-inner-h (- *popup-max-height* 4))
  (vte/resize vte *popup-vte-scrollback-rows* max-inner-w)
  ;; The captured output is piped (not a real pty), so it is plain Unix text
  ;; with bare \n — no kernel tty driver is present to translate that to \r\n.
  ;; This VTE is a faithful raw terminal emulator, so \n alone is just a
  ;; linefeed and does NOT return the cursor to column 0; without this, every
  ;; line after the first staircases further right.
  (vte/advance-bytes vte (string-replace output "\n" "\r\n"))

  (define title (if error? " error " " julia "))
  ;; NB: `theme-scope`'s Scheme wrapper injects *helix.cx* itself —
  ;; `(theme-scope "error")`, one argument. The deprecated `theme->fg`/
  ;; `theme->bg` are direct aliases and take the context explicitly, so they
  ;; read `(theme->fg *helix.cx*)`. Passing the context to `theme-scope` as well
  ;; is an ArityMismatch at load time; use one form throughout to avoid it.
  (define border
    (if error?
        ;; The theme's own error colour rather than a hardcoded red, so it sits
        ;; with the rest of the editor. A theme without the scope yields a
        ;; default Style, which is simply the unstyled border.
        (theme-scope "error")
        (theme-scope "ui.text")))

  ;; Size to the content. Measured AFTER feeding, so the VTE's own wrapping is
  ;; already accounted for; clamped to the max box, and floored wide enough for
  ;; the title. Anything past the visible rows becomes the overflow count rather
  ;; than being silently dropped, which is what used to happen.
  (define extent (vte-extent vte))
  (define content-h (max 1 (cdr extent)))
  (define visible-h (min max-inner-h content-h))
  (define inner-w
    (clamp (max *popup-min-inner-width* (string-length title))
           max-inner-w
           (car extent)))
  (define popup
    (new-component! "martensite-output"
                    (OutputPopup vte
                                 (+ inner-w 4)
                                 (+ visible-h 4)
                                 (let ([cursor (car (current-cursor))])
                                   (if cursor (+ 1 (position-row cursor)) 0))
                                 (let ([cursor (car (current-cursor))])
                                   (if cursor (position-col cursor) 0))
                                 border
                                 title
                                 (- content-h visible-h))
                    output-popup-render
                    (hash "handle_event" output-popup-handle-event)))
  (push-component! popup))

;; ─── Dispatch ────────────────────────────────────────────────────────────────

;; Collapse a multi-line message into something the one-row status bar can show.
(define (one-line text)
  (define t (trim text))
  (if (equal? t "")
      "jld failed with no output"
      (trim (car (split-many t "\n")))))

;; jld prints a three-line banner to stderr when it has to cold-start a daemon,
;; and we merge stderr into the captured output because that is where Julia
;; writes errors. The banner would otherwise turn the first `1+1` of a session
;; into a full-size popup of progress messages with the answer at the bottom.
;;
;; Matched on these three literals rather than the general `jld: ` prefix on
;; purpose: jld says other things with that prefix — "Revise failed to apply
;; changes" among them — which mean the output came from stale code and must NOT
;; be hidden. Only the startup banner is dropped.
(define *jld-startup-banner*
  (list "jld: verifying" "jld: starting daemon" "jld: daemon ready"))

;; findf returns the matching element (or #false); Steel has no `any?`.
(define (startup-banner-line? line)
  (if (findf (lambda (prefix) (starts-with? line prefix)) *jld-startup-banner*)
      #t
      #f))

(define (strip-startup-banner text)
  (trim (string-join
          (filter (lambda (line) (not (startup-banner-line? line)))
                  (split-many text "\n"))
          "\n")))

;; Widest result that goes to the status bar instead of a popup. Deliberately
;; conservative: the status line is one row and shares it with the mode
;; indicator, so anything near a full width would be truncated by Helix.
(define *status-inline-max* 72)

;; A short, single-line, successful result does not deserve a floating window —
;; `2` in a 60x10 box was the old behaviour. Errors always get the popup, so the
;; frame colour still carries the signal, and multi-line output always does, so
;; structure is preserved.
(define (inline-result? result body)
  (and (equal? (JldResult-code result) 0)
       (not (equal? body ""))
       (equal? (length (split-many body "\n")) 1)
       (<= (string-length body) *status-inline-max*)))

(define (report! result status)
  (define body (strip-startup-banner (JldResult-output result)))
  (cond
    [(equal? body "") (set-status! status)]
    [(inline-result? result body) (set-status! (string-append "julia: " body))]
    [else
     (set-status! status)
     (show-output! body (not (equal? (JldResult-code result) 0)))]))

;; The two modes are deliberately not interchangeable, and neither falls back to
;; the other:
;;
;;   'repl — jld eval-repl. The REPL evaluates, so the result lands in the
;;           developer's terminal and `ans` is set. The socket only acknowledges
;;           the paste, so there is nothing here to show.
;;   'eval — jld eval. We evaluate, so the result comes back and lands in the
;;           popup, and the prompt is never touched.
;;
;; An earlier version silently retried a failed paste as a captured eval. That
;; blurred the one distinction the two commands exist to express — pick the
;; command that matches where you want the answer, and a missing REPL is an
;; error to fix rather than a mode to switch into.
(define (send-code-inner! code mode)
  (cond
    [(equal? mode 'eval)
     (define result (jld-eval code))
     (hx.with-context (lambda () (report! result "martensite: evaluated")))]
    [else
     (define result (jld-eval-repl code))
     (hx.with-context
       (lambda ()
         (if (equal? (JldResult-code result) 0)
             (set-status! "martensite: sent to REPL")
             ;; jld's own message is already specific and actionable ("no REPL
             ;; attached to <id>; start one with `jld connect`"), so pass it
             ;; through rather than inventing a vaguer one. Flattened to a
             ;; single line for the status bar.
             (set-error! (string-append "martensite: " (one-line (JldResult-output result)))))))]))

;; Errors on the background thread otherwise vanish silently —
;; spawn-native-thread has no visible failure path — so surface whatever goes
;; wrong via set-error! instead of guessing blind.
(define (send-code! code mode)
  (spawn-native-thread
    (lambda ()
      (with-handler
        (lambda (e)
          (define msg (call-with-output-string (lambda (port) (display e port))))
          (hx.with-context (lambda () (set-error! (string-append "martensite error: " msg)))))
        (send-code-inner! code mode))))
  (set-status! "martensite: sending…"))

;; ─── Code extraction ─────────────────────────────────────────────────────────

(define (selection-code)
  (string-join (register->value #\.) "\n"))

;; Walk up the tree-sitter tree until we reach a direct child of the root.
(define (find-top-level-node node)
  (define parent (tsnode-parent node))
  (cond
    [(not parent) node]
    [(not (tsnode-parent parent)) node]
    [else (find-top-level-node parent)]))

;; Returns the top-level form's text, or a (cons 'warn message) to report.
(define (top-level-code)
  (define doc-id (editor->doc-id (editor-focus)))
  (define tree (document->tree doc-id))
  (cond
    [(not tree) (cons 'warn "martensite: no tree-sitter tree for this buffer")]
    [else
     (define rope (editor->text doc-id))
     (define cursor-byte (rope-char->byte rope (cursor-position)))
     (define node-at-cursor
       (tsnode-named-descendant-byte-range (tstree->root tree) cursor-byte cursor-byte))
     (cond
       [(not node-at-cursor) (cons 'warn "martensite: no node at cursor")]
       [else
        (define top-node (find-top-level-node node-at-cursor))
        (rope->string (rope->byte-slice rope
                                        (tsnode-start-byte top-node)
                                        (tsnode-end-byte top-node)))])]))

(define (dispatch-selection! mode)
  (define code (selection-code))
  (if (or (not code) (equal? code ""))
      (set-warning! "martensite: nothing selected")
      (send-code! code mode)))

(define (dispatch-top-level! mode)
  (define code (top-level-code))
  (cond
    [(pair? code) (set-warning! (cdr code))]
    [(or (not code) (equal? code "")) (set-warning! "martensite: top-level node is empty")]
    [else (send-code! code mode)]))

;; ─── Commands ────────────────────────────────────────────────────────────────

;;@doc
;; Paste the current selection into the live Julia REPL, as if typed there:
;; echoed at the prompt, `ans` set, result shown in the developer's terminal.
;; Falls back to a captured eval (result in a popup) if no REPL is attached.
(define (send-to-julia-repl)
  (dispatch-selection! 'repl))

;;@doc
;; Paste the top-level tree-sitter form under the cursor into the live Julia
;; REPL, as if typed there.
(define (send-top-level-to-julia-repl)
  (dispatch-top-level! 'repl))

;;@doc
;; Evaluate the current selection in the Julia session without touching the
;; REPL prompt; the result is shown in a popup.
(define (eval-in-julia)
  (dispatch-selection! 'eval))

;;@doc
;; Evaluate the top-level tree-sitter form under the cursor without touching
;; the REPL prompt; the result is shown in a popup.
(define (eval-top-level-in-julia)
  (dispatch-top-level! 'eval))
