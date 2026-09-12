;;; sqlitefs.el --- Browse sqlitefs repos from Emacs -*- lexical-binding: t; -*-

;; Read-only browsing of sqlitefs repositories (single-file, versioned,
;; content-addressed — see https://github.com/nagy/sqlitefs) via the
;; `sqlitefs' Emacs dynamic module. No FUSE mount needed.

;;; Commentary:

;; Load the module once (the cdylib is built with
;; `cargo build --features emacs'):

;;   (setq sqlitefs-module-path "/path/to/target/debug/libsqlitefs.so")
;;   (require 'sqlitefs)

;; Then:

;;   M-x sqlitefs-dired       browse a repo's tree (main ref), like Dired
;;   M-x sqlitefs-visit       open a repo file in a read-only buffer
;;   M-x sqlitefs-show-refs   list refs with their commit CIDs

;;; Code:

(require 'map)
(require 'subr-x)

;; Functions defined by the dynamic module (src/emacs_module.rs).
(declare-function sqlitefs-refs "emacs_module" (path))
(declare-function sqlitefs-ls "emacs_module" (path dir &optional ref))
(declare-function sqlitefs-read "emacs_module" (path file &optional ref))
(declare-function sqlitefs-log "emacs_module" (path &optional ref))

(defgroup sqlitefs nil
  "Read-only Emacs access to sqlitefs repositories."
  :group 'files
  :prefix "sqlitefs-")

(defcustom sqlitefs-module-path nil
  "Explicit path to the sqlitefs dynamic module (libsqlitefs.so).
When nil, `sqlitefs--ensure-module' searches `load-path' for the
library (the Nix-installed emacs-sqlitefs package puts the module
next to this file), then falls back to the `target/{debug,release}'
directories of a dev checkout."
  :type '(choice (const :tag "auto" nil) file))

(defvar-local sqlitefs--db nil)
(defvar-local sqlitefs--ref nil)
(defvar-local sqlitefs--dir nil)

(defun sqlitefs--locate-module ()
  "Return the absolute path of the module library, or nil."
  (or sqlitefs-module-path
      ;; Nix-installed package: the .so lives next to the elisp.
      (locate-file "libsqlitefs.so" load-path)
      ;; Dev checkout: cargo's target directories next to this file.
      (when-let* ((dir (when-let* ((lib (locate-library "sqlitefs")))
                         (file-name-directory lib))))
        (seq-some
         (lambda (profile)
           (let ((p (expand-file-name
                     (concat "target/" profile "/libsqlitefs.so") dir)))
             (and (file-exists-p p) p)))
         '("debug" "release")))))

(defun sqlitefs--ensure-module ()
  "Load the sqlitefs dynamic module unless its functions exist already."
  (unless (fboundp 'sqlitefs-refs)
    (module-load
     (or (sqlitefs--locate-module)
         (error "sqlitefs module (libsqlitefs.so) not found; set `sqlitefs-module-path' or add it to `load-path'")))))

(defun sqlitefs-refs-alist (db)
  "Return refs of repo DB as an alist ((NAME . CID) ...)."
  (sqlitefs--ensure-module)
  (sqlitefs-refs (expand-file-name db)))

;;;###autoload
(defun sqlitefs-show-refs (db)
  "Show the refs of sqlitefs repo DB in a buffer."
  (interactive "fRepository (SQLite file): ")
  (with-current-buffer (get-buffer-create "*sqlitefs refs*")
    (let ((inhibit-read-only t))
      (erase-buffer)
      (pcase-dolist (`(,name . ,cid) (sqlitefs-refs-alist db))
        (insert (format "%s %s\n" cid name))))
    (special-mode))
  (pop-to-buffer "*sqlitefs refs*"))

(defun sqlitefs-ls-entries (db dir &optional ref)
  "List DIR of repo DB (REF, default \"main\") as parsed entries."
  (sqlitefs--ensure-module)
  (sqlitefs-ls (expand-file-name db) dir (or ref "main")))

(defun sqlitefs-read-string (db file &optional ref)
  "Return content of FILE in repo DB (REF, default \"main\")."
  (sqlitefs--ensure-module)
  (sqlitefs-read (expand-file-name db) file (or ref "main")))

;;;###autoload
(defun sqlitefs-visit (db file &optional ref)
  "Open FILE from repo DB (REF, default \"main\") in a read-only buffer."
  (interactive "fRepository (SQLite file):
GFile in repo: ")
  (let ((content (sqlitefs-read-string db file ref))
        (name (format "/sqlitefs:%s:%s" (file-name-nondirectory db) file)))
    (with-current-buffer (get-buffer-create name)
      (let ((inhibit-read-only t))
        (erase-buffer)
        (insert content)
        (goto-char (point-min)))
      (setq buffer-read-only t)
      (setq-local sqlitefs--db (expand-file-name db))
      (setq-local sqlitefs--ref (or ref "main"))
      (setq-local sqlitefs--dir file))
    (pop-to-buffer name)))

(defun sqlitefs--refresh ()
  "Re-render the sqlitefs-dired buffer for the current ref and dir."
  (let* ((inhibit-read-only t)
         (entries (sqlitefs-ls-entries sqlitefs--db sqlitefs--dir sqlitefs--ref))
         (entries (sort entries
                        (lambda (a b)
                          (or (and (string= (nth 1 a) "dir")
                                   (not (string= (nth 1 b) "dir")))
                              (and (string= (nth 1 a) (nth 1 b))
                                   (string-lessp (car a) (car b))))))))
    (erase-buffer)
    (insert (format "sqlitefs: %s @ %s:%s\n\n"
                    (abbreviate-file-name sqlitefs--db)
                    sqlitefs--ref
                    (if (string-empty-p sqlitefs--dir) "/" sqlitefs--dir)))
    (unless (string-empty-p sqlitefs--dir)
      (insert "  ../\n"))
    (pcase-dolist (`(,name ,kind ,size ,mode) entries)
      (insert-text-button name
                          'follow-link t
                          'face (pcase kind
                                  ("dir" 'dired-directory)
                                  ("symlink" 'dired-symlink-face)
                                  (_ 'default))
                          'sqlitefs--name name
                          'sqlitefs--kind kind)
      (insert (format "\t%s\t%s\t%s\n" kind size
                      (if mode (format "%o" mode) ""))))
    (goto-char (point-min))))

(defun sqlitefs--open-thing ()
  "Open the entry at point: navigate to dirs, visit files."
  (interactive)
  (if-let* ((name (get-text-property (point) 'sqlitefs--name))
            (kind (get-text-property (point) 'sqlitefs--kind)))
      (pcase kind
        ("dir"
         (setq-local sqlitefs--dir
                     (concat sqlitefs--dir "/" name))
         (sqlitefs--refresh))
        ("file"
         (sqlitefs-visit sqlitefs--db
                         (concat (string-remove-prefix "/" sqlitefs--dir)
                                 "/" name)
                         sqlitefs--ref))
        ("symlink" (message "symlink (not followed in read-only mode)")))
    (setq-local sqlitefs--dir
                (string-remove-suffix "/" (or (file-name-directory sqlitefs--dir) "")))
    (sqlitefs--refresh)))

(defun sqlitefs--up ()
  "Go to the parent directory."
  (interactive)
  (setq-local sqlitefs--dir
              (string-remove-suffix "/"
                                    (or (file-name-directory
                                         (string-remove-prefix "/" sqlitefs--dir))
                                        "")))
  (sqlitefs--refresh))

(defun sqlitefs--commit ()
  "Show the current directory in the buffer title (placeholder)."
  (interactive)
  (message "sqlitefs: auto-commit happens in the FUSE mount / CLI; this buffer is read-only"))

(defvar-keymap sqlitefs-dired-mode-map
  "RET" #'sqlitefs--open-thing
  "g"   #'sqlitefs--refresh
  "^"   #'sqlitefs--up
  "q"   #'quit-window)

(define-derived-mode sqlitefs-dired-mode special-mode "sqlitefs-dired"
  "Read-only Dired-like browsing of a sqlitefs repository."
  (setq-local revert-buffer-function (lambda (&rest _) (sqlitefs--refresh))))

;;;###autoload
(defun sqlitefs-dired (db)
  "Browse the sqlitefs repo DB (main ref) in a Dired-like buffer."
  (interactive "fRepository (SQLite file): ")
  (sqlitefs--ensure-module)
  (let ((buf (get-buffer-create
              (format "*sqlitefs: %s*" (file-name-nondirectory db)))))
    (with-current-buffer buf
      (setq-local sqlitefs--db (expand-file-name db))
      (setq-local sqlitefs--ref "main")
      (setq-local sqlitefs--dir "")
      (sqlitefs--refresh)
      (sqlitefs-dired-mode))
    (pop-to-buffer buf)))

(provide 'sqlitefs)
;;; sqlitefs.el ends here
