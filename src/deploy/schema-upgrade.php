<?php
/**
 * magebuild: apply only Magento's declarative schema diff (the DDL part of
 * setup:db-schema:upgrade, without patches or setup scripts), and only when it
 * is exactly the set of changes magebuild classified as safe to run online.
 *
 * Usage: php schema-upgrade.php <magento root> < expected
 *   expected: one "<operation> <table>[.<element>]" per line.
 * Exit 0: applied. Exit 3: Magento's diff differs from the expected list, so
 * nothing was changed (both lists go to stderr). Anything else: an error.
 */
declare(strict_types=1);

use Magento\Framework\App\Bootstrap;
use Magento\Framework\Setup\Declaration\Schema\Diff\SchemaDiff;
use Magento\Framework\Setup\Declaration\Schema\Dto\Table;
use Magento\Framework\Setup\Declaration\Schema\OperationsExecutor;
use Magento\Framework\Setup\Declaration\Schema\SchemaConfigInterface;

$root = $argv[1] ?? '';
if ($root === '') {
    fwrite(STDERR, "usage: php schema-upgrade.php <magento root> < expected\n");
    exit(2);
}
$expected = array_values(array_filter(array_map('trim', explode("\n", stream_get_contents(STDIN)))));

require $root . '/app/bootstrap.php';
$om = Bootstrap::create(BP, $_SERVER)->getObjectManager();

$config = $om->get(SchemaConfigInterface::class);
$diff = $om->get(SchemaDiff::class)->diff($config->getDeclarationConfig(), $config->getDbConfig());

$actual = [];
foreach ($diff->getAll() ?? [] as $operations) {
    foreach ($operations ?? [] as $operation => $histories) {
        foreach ($histories as $history) {
            $element = $history->getNew() ?? $history->getOld();
            $actual[] = $element instanceof Table
                ? "$operation {$element->getName()}"
                : "$operation {$element->getTable()->getName()}.{$element->getName()}";
        }
    }
}

sort($expected);
sort($actual);
if ($expected !== $actual) {
    fwrite(STDERR, "Magento's schema diff is not the one magebuild classified; nothing changed.\n");
    fwrite(STDERR, "  only in Magento's: " . implode(', ', array_diff($actual, $expected)) . "\n");
    fwrite(STDERR, "  only in magebuild's: " . implode(', ', array_diff($expected, $actual)) . "\n");
    exit(3);
}
if ($actual === []) {
    exit(0);
}
$om->get(OperationsExecutor::class)->execute($diff, []);
echo count($actual), " schema change(s) applied\n";
