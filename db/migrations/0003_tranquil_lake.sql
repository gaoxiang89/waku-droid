ALTER TABLE `sessions` ADD `pinned` integer DEFAULT false NOT NULL;--> statement-breakpoint
ALTER TABLE `sessions` ADD `archived` integer DEFAULT false NOT NULL;