;;; emacs-module-tests.el --- ERT tests for the sqlitefs Emacs module -*- lexical-binding: t; -*-

;; Run from the repo root after building the module:
;;   cargo build --features emacs
;;   emacs -Q --batch -l test/emacs-module-tests.el
;;
;; Nix installCheck overrides the locations via env:
;;   SQLITEFS_TEST_BIN     sqlitefs CLI binary
;;   SQLITEFS_TEST_MODULE  libsqlitefs.so dynamic module

(require 'ert)
(require 'cl-lib)

(defconst sqlitefs-test-root
  ;; repo root: the directory containing this test/ file
  (file-name-directory
   (directory-file-name (file-name-directory load-file-name))))

(defconst sqlitefs-test-target-dir
  (file-name-as-directory
   (expand-file-name (concat (or (getenv "CARGO_TARGET_DIR") "target") "/debug")
                     sqlitefs-test-root)))

(defconst sqlitefs-test-bin
  (or (getenv "SQLITEFS_TEST_BIN")
      (expand-file-name "sqlitefs" sqlitefs-test-target-dir)))

(defconst sqlitefs-test-module
  (or (getenv "SQLITEFS_TEST_MODULE")
      (expand-file-name "libsqlitefs.so" sqlitefs-test-target-dir)))

;; Scratch repo: a fresh temp dir per run (the Nix sandbox cannot write
;; next to the test sources).
(defconst sqlitefs-test-dir
  (file-name-as-directory (make-temp-file "sqlitefs-tests-" t)))

(defconst sqlitefs-test-db
  (expand-file-name "repo.db" sqlitefs-test-dir))

(defun sqlitefs-test--run (&rest args)
  (let ((exit (apply #'call-process sqlitefs-test-bin nil t nil args)))
    (should (= exit 0))))

(defun sqlitefs-test--setup ()
  "Build a fresh repo with hello.txt, sub/run.sh, and a tagged ref."
  (delete-directory sqlitefs-test-dir 'recursive)
  (make-directory sqlitefs-test-dir 'recursive)
  ;; hello.txt on the host
  (with-temp-file (expand-file-name "hello.txt" sqlitefs-test-dir)
    (insert "hello emacs\n"))
  ;; sub/run.sh on the host
  (make-directory (expand-file-name "sub" sqlitefs-test-dir))
  (with-temp-file (expand-file-name "sub/run.sh" sqlitefs-test-dir)
    (insert "#!/bin/sh\necho hi\n"))
  (set-file-modes (expand-file-name "sub/run.sh" sqlitefs-test-dir) #o755)
  (sqlitefs-test--run "init" sqlitefs-test-db)
  (sqlitefs-test--run "cp" sqlitefs-test-db
                      (expand-file-name "hello.txt" sqlitefs-test-dir)
                      "hello.txt")
  (sqlitefs-test--run "cp" sqlitefs-test-db
                      (expand-file-name "sub/run.sh" sqlitefs-test-dir)
                      "sub/run.sh")
  ;; tag a second ref (empty snapshot name via touch semantics → manual ref
  ;; by re-committing is not exposed; tag creates a named ref via the CLI)
  (sqlitefs-test--run "show-ref" sqlitefs-test-db))

(defun sqlitefs-test--module ()
  "Load the module cdylib (once; re-init would error)."
  (unless (fboundp 'sqlitefs-refs)
    (module-load sqlitefs-test-module)))

;;; Catalog parity

(defun sqlitefs-test--check-catalog-entry (name args doc)
  "Assert that catalog entry (NAME ARGS DOC) matches the real function."
  (let* ((sym (intern name))
         (arity (func-arity (symbol-function sym))))
    (should (fboundp sym))
    (should (= (car arity) (length args)))
    (should (= (cdr arity) (length args)))
    (should (string-prefix-p doc (documentation sym t)))))

(ert-deftest sqlitefs-module-catalog-test ()
  (sqlitefs-test--setup)
  (sqlitefs-test--module)
  ;; Known surface: the catalog pins the full exported set.
  (should (equal (mapcar #'car (sqlitefs-catalog))
                 '("sqlitefs-refs" "sqlitefs-ls" "sqlitefs-read"
                   "sqlitefs-log" "sqlitefs-catalog")))
  ;; Parity: every entry is fboundp with matching arity and doc first line.
  (pcase-dolist (`(,name ,args ,doc) (sqlitefs-catalog))
    (sqlitefs-test--check-catalog-entry name args doc)))

;;; Tests

(ert-deftest sqlitefs-module-refs-test ()
  (sqlitefs-test--setup)
  (sqlitefs-test--module)
  (let ((refs (sqlitefs-refs sqlitefs-test-db)))
    (should (= (length refs) 1))
    (pcase-let ((`(,name . ,cid) (car refs)))
      (should (string= name "main"))
      (should (string-prefix-p "bafy" cid)))))

(ert-deftest sqlitefs-module-ls-root-test ()
  (sqlitefs-test--setup)
  (sqlitefs-test--module)
  (let ((entries (sqlitefs-ls sqlitefs-test-db "/" "main")))
    (should (= (length entries) 2))
    (let ((hello (cl-find "hello.txt" entries :key #'car :test #'string=))
          (sub (cl-find "sub" entries :key #'car :test #'string=)))
      (should hello)
      (should sub)
      ;; (NAME KIND SIZE MODE): hello.txt = 0o644 = 420
      (should (string= (nth 1 hello) "file"))
      (should (= (nth 2 hello) 12))
      (should (= (nth 3 hello) #o644))
      (should (string= (nth 1 sub) "dir")))))

(ert-deftest sqlitefs-module-ls-nested-test ()
  (sqlitefs-test--setup)
  (sqlitefs-test--module)
  (let ((entries (sqlitefs-ls sqlitefs-test-db "/sub" "main")))
    (should (= (length entries) 1))
    (let ((run (car entries)))
      (should (string= (car run) "run.sh"))
      (should (string= (nth 1 run) "file"))
      (should (= (nth 3 run) #o755)))))

(ert-deftest sqlitefs-module-read-test ()
  (sqlitefs-test--setup)
  (sqlitefs-test--module)
  (should (string= (sqlitefs-read sqlitefs-test-db "hello.txt" "main")
                   "hello emacs\n"))
  (should (string= (sqlitefs-read sqlitefs-test-db "sub/run.sh" "main")
                   "#!/bin/sh\necho hi\n")))

(ert-deftest sqlitefs-module-log-test ()
  (sqlitefs-test--setup)
  (sqlitefs-test--module)
  (let ((log (sqlitefs-log sqlitefs-test-db "main")))
    (should (>= (length log) 3))            ; one commit per cp + init
    (pcase-let ((`(,cid . ,ts) (car log)))
      (should (string-prefix-p "bafy" cid))
      (should (numberp ts)))))

(ert-deftest sqlitefs-module-errors-test ()
  (sqlitefs-test--setup)
  (sqlitefs-test--module)
  (should-error (sqlitefs-read sqlitefs-test-db "nope" "main"))
  (should-error (sqlitefs-ls sqlitefs-test-db "/hello.txt" "main")))

(ert-deftest sqlitefs-elisp-visit-test ()
  (sqlitefs-test--setup)
  (sqlitefs-test--module)
  (add-to-list 'load-path (expand-file-name "emacs" sqlitefs-test-root))
  (require 'sqlitefs)
  (sqlitefs-visit sqlitefs-test-db "hello.txt" "main")
  (with-current-buffer "/sqlitefs:repo.db:hello.txt"
    (should (string= (buffer-string) "hello emacs\n"))
    (should buffer-read-only)))

(ert-deftest sqlitefs-elisp-dired-test ()
  (sqlitefs-test--setup)
  (sqlitefs-test--module)
  (add-to-list 'load-path (expand-file-name "emacs" sqlitefs-test-root))
  (require 'sqlitefs)
  (sqlitefs-dired sqlitefs-test-db)
  (with-current-buffer "*sqlitefs: repo.db*"
    (should (string-match "hello.txt" (buffer-string)))
    (should (string-match "sub" (buffer-string)))))

(ert-run-tests-batch-and-exit)
