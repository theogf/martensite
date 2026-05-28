;;; julia-remoterepl.scm
;;; Steel plugin for Helix: send current selection to a DaemonicCabal.jl server.
;;;
;;; Requires `juliaclient` to be on PATH.
;;; Default keybinding: C-j (normal and select modes), C-S-j for top-level

(require "helix/misc.scm")        ; set-status!, set-warning!, set-error!, cursor-position
(require "helix/static.scm")      ; current-selection->string, get-helix-cwd
(require "helix/editor.scm")      ; set-register!, editor-focus, editor->doc-id, editor->text
(require "helix/ext.scm")         ; hx.with-context, spawn-native-thread
(require "helix/treesitter.scm")  ; document->tree, tstree->root, tsnode-*
(require (prefix-in helix. "helix/commands.scm")) ; helix.vsplit, helix.open

(require-builtin steel/process)      ; command, with-stdin, with-stdout-piped, spawn-process, wait->stdout
(require-builtin steel/filesystem)   ; path-exists?, delete-file!
(require-builtin helix/core/text) ; rope-char->byte, rope->byte-slice, rope->string
(require "steel/result")          ; Ok?, unwrap-ok

(provide send-to-julia-repl)
(provide send-top-level-to-julia-repl)

(define *julia-output-file* "/tmp/julia-steel-output.txt")

;; ─── Session ─────────────────────────────────────────────────────────────────

;; Session cascade: .juliasession file → zellij tab name → CWD.
(define (get-session)
  (define session-file (string-append (get-helix-cwd) "/.juliasession"))
  (cond
    [(path-exists? session-file)
     (define port (open-input-file session-file))
     (define name (trim (read-line port)))
     (close-input-port port)
     name]
    [else
     (define zellij-name
       (~> (command "sh" (list "-c" "zellij action current-tab-info | head -1 | cut -c7-"))
           with-stdout-piped
           spawn-process
           unwrap-ok
           wait->stdout
           unwrap-ok
           trim))
     (if (equal? zellij-name "") (get-helix-cwd) zellij-name)]))

;; ─── Sending code ────────────────────────────────────────────────────────────

;; Send code via --eval so DaemonWorker can echo it in the REPL (sync_echo_expressions).
;; --sync makes the code appear in the interactive REPL session as "julia> <code>".
(define (run-juliaclient session code)
  (define process
    (~> (command "juliaclient" (list (string-append "--session=" session) "--sync" "--eval" code))
        with-stdout-piped
        spawn-process
        unwrap-ok))
  (unwrap-ok (wait->stdout process)))

;; Show output in a vsplit buffer, or just update the status bar if empty.
(define (show-output! output)
  (when (path-exists? *julia-output-file*)
    (delete-file! *julia-output-file*))
  (define out (open-output-file *julia-output-file*))
  (write-string output out)
  (close-output-port out)
  (helix.vsplit)
  (helix.open *julia-output-file*))

;; Send code string to juliaclient and display output.
(define (send-code! code)
  (define session (get-session))
  (spawn-native-thread
    (lambda ()
      (define output (run-juliaclient session code))
      (hx.with-context
        (lambda ()
          (if (equal? output "")
              (set-status! "julia-remoterepl: done (no output)")
              (show-output! output))))))
  (set-status! "julia-remoterepl: sending…"))

;; ─── Main commands ───────────────────────────────────────────────────────────

;;@doc
;; Send the current selection to the running DaemonicCabal.jl server.
;; On failure, copies a server startup command to the clipboard.
(define (send-to-julia-repl)
  (define code (string-join (register->value #\.) "\n"))
  (cond
    [(or (not code) (equal? code ""))
     (set-warning! "julia-remoterepl: nothing selected")]
    [else
     (send-code! code)]))

;; Walk up the tree-sitter tree until we reach a direct child of the root.
(define (find-top-level-node node)
  (define parent (tsnode-parent node))
  (cond
    [(not parent) node]
    [(not (tsnode-parent parent)) node]
    [else (find-top-level-node parent)]))

;;@doc
;; Send the top-level tree-sitter form under the cursor to the running
;; DaemonicCabal.jl server. On failure, copies a startup command to the clipboard.
(define (send-top-level-to-julia-repl)
  (define doc-id (editor->doc-id (editor-focus)))
  (define tree (document->tree doc-id))
  (cond
    [(not tree)
     (set-warning! "julia-remoterepl: no tree-sitter tree for this buffer")]
    [else
     (define rope (editor->text doc-id))
     (define cursor-char (cursor-position))
     (define cursor-byte (rope-char->byte rope cursor-char))
     (define root (tstree->root tree))
     (define node-at-cursor
       (tsnode-named-descendant-byte-range root cursor-byte cursor-byte))
     (cond
       [(not node-at-cursor)
        (set-warning! "julia-remoterepl: no node at cursor")]
       [else
        (define top-node (find-top-level-node node-at-cursor))
        (define start-byte (tsnode-start-byte top-node))
        (define end-byte (tsnode-end-byte top-node))
        (define code (rope->string (rope->byte-slice rope start-byte end-byte)))
        (cond
          [(or (not code) (equal? code ""))
           (set-warning! "julia-remoterepl: top-level node is empty")]
          [else
           (send-code! code)])])]))
