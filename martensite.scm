;;; martensite.scm
;;; Steel plugin for Helix: send current selection to a DaemonicCabal.jl server.
;;;
;;; Requires `temper` to be on PATH.
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

(define *julia-output-file* "/tmp/martensite-output.txt")

;; ─── Sending code ────────────────────────────────────────────────────────────

;; temper resolves the session and passes --sync --eval to juliaclient.
(define (run-temper code)
  (define process
    (~> (command "temper" (list code))
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

;; Send code string via temper and display output.
(define (send-code! code)
  (spawn-native-thread
    (lambda ()
      (define output (run-temper code))
      (hx.with-context
        (lambda ()
          (if (equal? output "")
              (set-status! "martensite: done (no output)")
              (show-output! output))))))
  (set-status! "martensite: sending…"))

;; ─── Main commands ───────────────────────────────────────────────────────────

;;@doc
;; Send the current selection to the running DaemonicCabal.jl server.
;; On failure, copies a server startup command to the clipboard.
(define (send-to-julia-repl)
  (define code (string-join (register->value #\.) "\n"))
  (cond
    [(or (not code) (equal? code ""))
     (set-warning! "martensite: nothing selected")]
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
     (set-warning! "martensite: no tree-sitter tree for this buffer")]
    [else
     (define rope (editor->text doc-id))
     (define cursor-char (cursor-position))
     (define cursor-byte (rope-char->byte rope cursor-char))
     (define root (tstree->root tree))
     (define node-at-cursor
       (tsnode-named-descendant-byte-range root cursor-byte cursor-byte))
     (cond
       [(not node-at-cursor)
        (set-warning! "martensite: no node at cursor")]
       [else
        (define top-node (find-top-level-node node-at-cursor))
        (define start-byte (tsnode-start-byte top-node))
        (define end-byte (tsnode-end-byte top-node))
        (define code (rope->string (rope->byte-slice rope start-byte end-byte)))
        (cond
          [(or (not code) (equal? code ""))
           (set-warning! "martensite: top-level node is empty")]
          [else
           (send-code! code)])])]))
